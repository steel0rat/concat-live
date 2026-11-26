// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_export::ExportClip;
use concat_host::object_follow::{
    self, FollowError, FollowRequest, FollowResult, FollowSample, ObjectOffset, ObjectRegion,
};
use concat_project::model::{
    Clip, ClipKind, KeyEase, KeyProperty, MediaItem, Timeline, VideoSettings,
};
use concat_project::{Command, DocumentSettings};
use slint::ComponentHandle;

use crate::format::frames_timecode;
use crate::host::{self, project_epoch};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, ObjectFollowUi};

#[derive(Default)]
pub(crate) struct ObjectFollowState {
    selecting: Option<Target>,
    job: Option<Arc<AtomicBool>>,
    progress: f32,
}

impl ObjectFollowState {
    fn cancel(&mut self) {
        self.selecting = None;
        if let Some(job) = self.job.take() {
            job.store(true, Ordering::Relaxed);
        }
        self.progress = 0.0;
    }

    fn has_job(&self, token: &Arc<AtomicBool>) -> bool {
        self.job.as_ref().is_some_and(|job| Arc::ptr_eq(job, token))
    }
}

impl Drop for ObjectFollowState {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Target {
    timeline: String,
    video: VideoSettings,
    graphic: Arc<Clip>,
    subject: Arc<Clip>,
    media: MediaItem,
    scene: Arc<Vec<ExportClip>>,
    start: f64,
    end: f64,
}

fn track_order(timeline: &Timeline, track_id: &str) -> Option<usize> {
    timeline
        .tracks
        .iter()
        .position(|track| track.id == track_id)
}

fn available(studio: &Studio) -> Option<(&Arc<Clip>, &Arc<Clip>, &MediaItem)> {
    let _session = studio.session.as_ref()?;
    if studio.voiceover.recording() {
        return None;
    }
    let [selected] = studio.selection.as_slice() else {
        return None;
    };
    let timeline = studio.timeline();
    let time = f64::from(studio.playhead);
    let visible_at = |clip: &Clip| {
        clip.start <= time
            && time < clip.start + clip.duration
            && timeline
                .track(&clip.track_id)
                .is_some_and(|track| track.visible)
    };
    let graphic = timeline.clips.iter().find(|clip| {
        clip.id == *selected
            && matches!(
                clip.kind,
                ClipKind::Text | ClipKind::Shape | ClipKind::Image
            )
            && visible_at(clip)
            && !studio.locked(&clip.track_id)
    })?;
    let above = track_order(timeline, &graphic.track_id)?;
    let (subject, media, _) = timeline
        .clips
        .iter()
        .filter_map(|clip| {
            if clip.kind != ClipKind::Video || !visible_at(clip) {
                return None;
            }
            let track = track_order(timeline, &clip.track_id)?;
            let media = studio.project().media_by_id(&clip.media_id)?;
            (track > above && !media.placeholder && !media.path.is_empty())
                .then_some((clip, media, track))
        })
        .min_by_key(|(_, _, track)| *track)?;
    Some((graphic, subject, media))
}

impl Target {
    fn at_anchor(&self, studio: &Studio) -> bool {
        studio.source.is_none()
            && (self.start - studio.preview_time()).abs() <= 1e-6
            && self.still_matches(studio)
    }

    fn still_matches(&self, studio: &Studio) -> bool {
        let timeline = studio.timeline();
        if timeline.id != self.timeline
            || timeline.video != self.video
            || studio.locked(&self.graphic.track_id)
            || timeline.clip(&self.graphic.id) != Some(self.graphic.as_ref())
            || timeline.clip(&self.subject.id) != Some(self.subject.as_ref())
            || studio.project().media_by_id(&self.media.id) != Some(&self.media)
        {
            return false;
        }
        let visible = |id: &str| timeline.track(id).is_some_and(|track| track.visible);
        visible(&self.graphic.track_id)
            && visible(&self.subject.track_id)
            && track_order(timeline, &self.graphic.track_id)
                .zip(track_order(timeline, &self.subject.track_id))
                .is_some_and(|(graphic, subject)| graphic < subject)
    }
}

fn isolated_scene(studio: &Studio, subject: &Clip) -> Option<Arc<Vec<ExportClip>>> {
    let session = studio.session.as_ref()?;
    let mut isolated = studio.project().clone();
    isolated
        .active_mut()
        .clips
        .retain(|clip| clip.id == subject.id);
    let scene = concat_export::flatten::flatten_timeline_in(
        &isolated,
        None,
        Some(Path::new(session.path())),
    );
    (!scene.is_empty()).then(|| Arc::new(scene))
}

pub(crate) fn selecting_preview(
    studio: &Studio,
) -> Option<(Arc<Vec<ExportClip>>, DocumentSettings)> {
    let target = studio.object_follow.selecting.as_ref()?;
    let session = studio.session.as_ref()?;
    target
        .at_anchor(studio)
        .then(|| (Arc::clone(&target.scene), session.settings()))
}

pub(crate) fn begin(studio: &mut Studio) {
    if studio.object_follow.job.is_some() || studio.object_follow.selecting.is_some() {
        return;
    }
    let Some((graphic, subject, media)) = available(studio) else {
        studio.notify(&t("objectFollow.chooseClips"), true);
        return;
    };
    let Some(scene) = isolated_scene(studio, subject) else {
        studio.notify(&t("objectFollow.chooseClips"), true);
        return;
    };
    let target = Target {
        timeline: studio.timeline().id.clone(),
        video: studio.timeline().video,
        graphic: Arc::clone(graphic),
        subject: Arc::clone(subject),
        media: media.clone(),
        scene,
        start: f64::from(studio.playhead),
        end: (graphic.start + graphic.duration).min(subject.start + subject.duration),
    };
    studio.stop_glide();
    studio.pause();
    studio.end_hover();
    studio.close_source();
    studio.object_follow.selecting = Some(target);
    studio.request_preview();
}

pub(crate) fn cancel(studio: &mut Studio) {
    let was_selecting = studio.object_follow.selecting.is_some();
    studio.object_follow.cancel();
    if was_selecting {
        studio.request_preview();
    }
}

pub(crate) fn choose_region(studio: &mut Studio, x1: f32, y1: f32, x2: f32, y2: f32) {
    let Some(region) =
        ObjectRegion::new(f64::from(x1), f64::from(y1), f64::from(x2), f64::from(y2))
    else {
        return;
    };
    let Some(target) = studio.object_follow.selecting.take() else {
        return;
    };
    studio.request_preview();
    if !target.at_anchor(studio) {
        studio.notify(&t("objectFollow.editChanged"), true);
        return;
    }
    let request = FollowRequest {
        clip: target.scene[0].clone(),
        video: target.video,
        region,
        start: target.start,
        end: target.end,
    };
    let epoch = project_epoch();
    let job = Arc::new(AtomicBool::new(false));
    studio.object_follow.job = Some(Arc::clone(&job));
    studio.object_follow.progress = 0.0;
    let worker = Arc::clone(&job);
    host::spawn_long_in_project(
        move || {
            object_follow::analyse(&request, &worker, |progress| {
                let token = Arc::clone(&worker);
                host::on_ui_in_project(epoch, move |studio, _, _| {
                    if studio.object_follow.has_job(&token) {
                        studio.object_follow.progress = progress;
                    }
                });
            })
        },
        move |studio, _, _, result| {
            if studio.object_follow.has_job(&job) {
                studio.object_follow.job = None;
                studio.object_follow.progress = 0.0;
                finish(studio, target, result);
            }
        },
    );
}

fn finish(studio: &mut Studio, target: Target, result: Result<FollowResult, FollowError>) {
    let result = match result {
        Ok(result) => result,
        Err(FollowError::Cancelled) => return,
        Err(FollowError::NoInterval) => {
            studio.notify(&t("objectFollow.noInterval"), true);
            return;
        }
        Err(FollowError::NoFeatures) => {
            studio.notify(&t("objectFollow.noFeatures"), true);
            return;
        }
        Err(FollowError::ReadFrame(error)) => {
            log::warn!("object tracking: could not read a frame: {error}");
            studio.notify(&t("objectFollow.readFailed"), true);
            return;
        }
    };
    if !target.still_matches(studio) {
        studio.notify(&t("objectFollow.editChanged"), true);
        return;
    }
    let lost = result
        .lost_at
        .map(|time| frames_timecode(time as f32, target.video.rate() as f32));
    let commands = offset_keys(&target.graphic, &result.samples);
    if commands.is_empty() {
        let message = lost.as_ref().map_or_else(
            || t("objectFollow.noInterval"),
            |time| tf("objectFollow.lostBeforeKeys", &[time]),
        );
        studio.notify(&message, true);
        return;
    }
    if studio.try_apply(Command::Batch { commands }).is_err() {
        studio.notify(&t("objectFollow.applyFailed"), true);
        return;
    }
    let message = lost.as_ref().map_or_else(
        || t("objectFollow.finished"),
        |time| tf("objectFollow.partial", &[time]),
    );
    studio.notify(&message, false);
}

fn offset_keys(clip: &Clip, samples: &[FollowSample]) -> Vec<Command> {
    if samples.len() < 2 || clip.duration <= 0.0 {
        return Vec::new();
    }
    let first = samples[0].time;
    let last = samples[samples.len() - 1].time;
    let local = |seconds: f64| ((seconds - clip.start) / clip.duration).clamp(0.0, 1.0);
    let mut commands = Vec::new();
    for property in [KeyProperty::OffsetX, KeyProperty::OffsetY] {
        let mut moments: Vec<_> = samples
            .iter()
            .map(|sample| sample.time)
            .chain(
                clip.keys
                    .iter()
                    .filter(|key| key.property == property)
                    .map(|key| clip.start + key.at * clip.duration)
                    .filter(|seconds| first <= *seconds && *seconds <= last),
            )
            .collect();
        moments.sort_by(f64::total_cmp);
        moments.dedup();
        for seconds in moments {
            let offset = offset_at(samples, seconds);
            let delta = if property == KeyProperty::OffsetX {
                offset.x
            } else {
                offset.y
            };
            let at = local(seconds);
            let ease = clip
                .key_at(property, at)
                .map_or(KeyEase::LINEAR, |index| clip.keys[index].ease);
            commands.push(Command::SetClipKey {
                clip_id: clip.id.clone(),
                property,
                at,
                value: clip.value_at(property, at) + delta,
                ease,
            });
        }
    }
    commands
}

fn offset_at(samples: &[FollowSample], seconds: f64) -> ObjectOffset {
    let right = samples
        .partition_point(|sample| sample.time < seconds)
        .clamp(1, samples.len() - 1);
    let a = samples[right - 1];
    let b = samples[right];
    let fraction = ((seconds - a.time) / (b.time - a.time)).clamp(0.0, 1.0);
    ObjectOffset {
        x: a.offset.x + (b.offset.x - a.offset.x) * fraction,
        y: a.offset.y + (b.offset.y - a.offset.y) * fraction,
    }
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let ui = app.global::<ObjectFollowUi>();
    ui.set_can_start(available(studio).is_some());
    ui.set_selecting(studio.object_follow.selecting.is_some());
    ui.set_running(studio.object_follow.job.is_some());
    ui.set_progress(studio.object_follow.progress);
}

#[cfg(test)]
mod tests {
    use concat_project::Editor;

    use super::*;

    #[test]
    fn applies_both_axes_to_an_existing_graphic_and_undoes_them_in_one_step() {
        let mut editor = Editor::with_video(VideoSettings::default());
        let id = editor
            .apply(Command::AddShapeClip {
                track_id: Some("T1".into()),
                above: false,
                start: 1.0,
                duration: Some(4.0),
                style: None,
                name: "Label".into(),
            })
            .unwrap()
            .created_id
            .unwrap();
        editor
            .apply(Command::SetClipKey {
                clip_id: id.clone(),
                property: KeyProperty::OffsetX,
                at: 0.5,
                value: 0.2,
                ease: KeyEase::IN_OUT,
            })
            .unwrap();
        editor
            .apply(Command::SetClipKey {
                clip_id: id.clone(),
                property: KeyProperty::Scale,
                at: 0.25,
                value: 1.1,
                ease: KeyEase::LINEAR,
            })
            .unwrap();
        let before = editor.project().active().clip(&id).unwrap().clone();
        let path = [
            FollowSample {
                time: 2.0,
                offset: ObjectOffset::default(),
            },
            FollowSample {
                time: 4.0,
                offset: ObjectOffset { x: -0.1, y: 0.04 },
            },
        ];
        editor
            .apply(Command::Batch {
                commands: offset_keys(&before, &path),
            })
            .unwrap();
        let graphic = editor.project().active().clip(&id).unwrap();
        assert!((graphic.value_at(KeyProperty::OffsetX, 0.5) - 0.15).abs() < 1e-9);
        assert!((graphic.value_at(KeyProperty::OffsetY, 0.75) - 0.04).abs() < 1e-9);
        let manual = graphic.key_at(KeyProperty::OffsetX, 0.5).unwrap();
        assert_eq!(graphic.keys[manual].ease, KeyEase::IN_OUT);
        assert_eq!(graphic.value_at(KeyProperty::Scale, 0.25), 1.1);
        assert!(editor.undo());
        assert_eq!(editor.project().active().clip(&id), Some(&before));
    }
}
