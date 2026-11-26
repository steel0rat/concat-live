// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::sync::Arc;

use concat_project::commands::{Command, NewMedia};
use concat_project::model::{Clip, ClipKind, MediaItem, MediaKind, Project, ranges::MAX_TIME};
use concat_project::{placement::TOUCH, speed::source_consumed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TakeError {
    Stale,
    Unsupported,
    SeparateAudio,
    Incompatible,
    UnknownLength,
    InvalidInPoint,
    TooShort,
    MissingStream,
}

pub(super) struct TakeSlot {
    timeline_id: String,
    pub clip: Arc<Clip>,
    pub original: MediaItem,
}

impl TakeSlot {
    pub fn capture(project: &Project, clip_id: &str) -> Result<Self, TakeError> {
        let timeline = project.active();
        let clip = timeline
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .ok_or(TakeError::Stale)?;
        if clip.detached_from.is_some()
            || timeline
                .clips
                .iter()
                .any(|other| other.detached_from.as_deref() == Some(clip_id))
        {
            return Err(TakeError::SeparateAudio);
        }
        let original = project
            .media_by_id(&clip.media_id)
            .ok_or(TakeError::Unsupported)?;
        let compatible = matches!(
            (clip.kind, original.kind),
            (ClipKind::Video, MediaKind::Video) | (ClipKind::Audio, MediaKind::Audio)
        );
        if !compatible || original.placeholder || original.path.is_empty() {
            return Err(TakeError::Unsupported);
        }
        Ok(Self {
            timeline_id: timeline.id.clone(),
            clip: Arc::clone(clip),
            original: original.clone(),
        })
    }

    pub fn matches(&self, project: &Project) -> bool {
        let timeline = project.active();
        timeline.id == self.timeline_id
            && timeline.clip(&self.clip.id) == Some(self.clip.as_ref())
            && project.media_by_id(&self.original.id) == Some(&self.original)
    }

    pub fn accepts(&self, media: &MediaItem) -> bool {
        media.id != self.original.id
            && media.kind == self.original.kind
            && !media.placeholder
            && !media.path.is_empty()
    }

    pub fn limit(&self, media: &MediaItem) -> Result<f64, TakeError> {
        if !self.accepts(media) {
            return Err(TakeError::Incompatible);
        }
        if let Some(index) = self.clip.audio_stream
            && !media
                .audio_tracks
                .iter()
                .any(|stream| stream.index == index)
        {
            return Err(TakeError::MissingStream);
        }
        let length = media
            .duration
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or(TakeError::UnknownLength)?;
        let covered = source_consumed(&self.clip, self.clip.duration);
        if !covered.is_finite() || covered <= 0.0 || covered > length + TOUCH {
            return Err(TakeError::TooShort);
        }
        Ok((length - covered).clamp(0.0, MAX_TIME))
    }

    fn checked_in_point(&self, media: &MediaItem, start: f64) -> Result<f64, TakeError> {
        let limit = self.limit(media)?;
        if !(0.0..=limit + TOUCH).contains(&start) {
            return Err(TakeError::InvalidInPoint);
        }
        Ok(start.min(limit))
    }

    pub fn preview_project(
        &self,
        project: &Project,
        alternative: Option<(&MediaItem, f64)>,
    ) -> Result<Project, TakeError> {
        if !self.matches(project) {
            return Err(TakeError::Stale);
        }
        let mut clip = (*self.clip).clone();
        if let Some((media, start)) = alternative {
            clip.source_start = self.checked_in_point(media, start)?;
            clip.media_id = media.id.clone();
            clip.enhanced = None;
        }
        clip.start = 0.0;
        clip.transition_in = None;
        clip.muted = None;
        clip.volume = 1.0;
        clip.voice = None;
        clip.filters.clear();
        clip.fade_in = 0.0;
        clip.fade_out = 0.0;
        let mut scratch = project.clone();
        let timeline = scratch.active_mut();
        for track in &mut timeline.tracks {
            if track.id == clip.track_id {
                track.muted = false;
                track.visible = true;
            }
        }
        timeline.clips = vec![Arc::new(clip)];
        Ok(scratch)
    }

    pub fn replacement(
        &self,
        project: &Project,
        media: &MediaItem,
        start: f64,
    ) -> Result<Command, TakeError> {
        if !self.matches(project) || project.media_by_id(&media.id) != Some(media) {
            return Err(TakeError::Stale);
        }
        if project
            .active()
            .clips
            .iter()
            .any(|clip| clip.detached_from.as_deref() == Some(&self.clip.id))
        {
            return Err(TakeError::SeparateAudio);
        }
        let source_start = self.checked_in_point(media, start)?;
        Ok(Command::ReplaceClipMedia {
            clip_id: self.clip.id.clone(),
            item: NewMedia {
                path: media.path.clone(),
                name: media.name.clone(),
                duration: media.duration,
                kind: media.kind,
                width: media.width,
                height: media.height,
                frame_rate: media.frame_rate,
                frame_rate_fraction: media.frame_rate_fraction.clone(),
                video_codec: media.video_codec.clone(),
                audio_codec: media.audio_codec.clone(),
                has_audio: media.has_audio,
                audio_tracks: media.audio_tracks.clone(),
                origin: media.origin,
                color_space: media.color_space,
            },
            source_start: Some(source_start),
            enhanced: false,
            offset: 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::commands::IdMint;
    use concat_project::model::SpeedPoint;

    fn fixture(kind: MediaKind) -> Project {
        let mut project = Project::default();
        for (index, id) in ["m1", "m2"].into_iter().enumerate() {
            project.media.push(MediaItem {
                id: id.into(),
                path: format!("/qa/take-{id}.mp4"),
                kind,
                name: id.into(),
                duration: Some([14.0, 8.0][index]),
                has_audio: true,
                ..Default::default()
            });
        }
        let timeline = project.active_mut();
        let track_id = timeline.tracks[0].id.clone();
        timeline.tracks[0].muted = true;
        let kind = if kind == MediaKind::Video {
            ClipKind::Video
        } else {
            ClipKind::Audio
        };
        let mut clip = Clip::blank("c1", &track_id, kind, "Voice", 9.0, 3.0);
        clip.media_id = "m1".into();
        clip.source_start = 2.0;
        clip.fade_in = 0.2;
        clip.volume = 0.30;
        timeline.clips.push(Arc::new(clip));
        project
    }

    #[test]
    fn previews_a_take_in_the_same_local_interval_with_audible_sound() {
        let project = fixture(MediaKind::Audio);
        let slot = TakeSlot::capture(&project, "c1").unwrap();
        let media = project.media_by_id("m2").unwrap();
        let audition = slot.preview_project(&project, Some((media, 1.25))).unwrap();
        let clip = audition.active().clip("c1").unwrap();
        assert_eq!(
            (clip.start, clip.duration, clip.source_start),
            (0.0, 3.0, 1.25)
        );
        assert_eq!(clip.media_id, "m2");
        assert_eq!(clip.volume, 1.0);
        assert!(!audition.active().track(&clip.track_id).unwrap().muted);
        assert_eq!(project.active().clip("c1").unwrap().media_id, "m1");
    }

    #[test]
    fn a_speed_curve_sets_the_amount_of_source_needed() {
        let mut project = fixture(MediaKind::Video);
        project.media[1].duration = Some(5.5);
        Arc::make_mut(&mut project.active_mut().clips[0]).speed_curve = Some(vec![
            SpeedPoint {
                at: 0.0,
                speed: 1.0,
                slope: None,
            },
            SpeedPoint {
                at: 1.0,
                speed: 3.0,
                slope: None,
            },
        ]);
        let slot = TakeSlot::capture(&project, "c1").unwrap();
        let media = project.media_by_id("m2").unwrap();
        assert_eq!(slot.limit(media), Err(TakeError::TooShort));
        assert_eq!(
            slot.replacement(&project, media, 0.0),
            Err(TakeError::TooShort)
        );
    }

    #[test]
    fn replacement_keeps_an_edit_and_its_id_and_refuses_stale_revisions() {
        let mut project = fixture(MediaKind::Video);
        let slot = TakeSlot::capture(&project, "c1").unwrap();
        let media = project.media_by_id("m2").unwrap().clone();
        let command = slot.replacement(&project, &media, 1.25).unwrap();
        let original = project.active().clip("c1").unwrap().clone();
        concat_project::commands::apply(&mut project, &mut IdMint::default(), command).unwrap();
        let mut wanted = original;
        wanted.media_id = "m2".into();
        wanted.source_start = 1.25;
        assert_eq!(project.active().clip("c1"), Some(&wanted));
        assert_eq!(project.media.len(), 2);
        assert_eq!(
            slot.replacement(&project, &media, 0.0),
            Err(TakeError::Stale)
        );
    }

    #[test]
    fn a_separately_detached_track_cannot_be_swapped_in_isolation() {
        let mut project = fixture(MediaKind::Video);
        let track = project.active().tracks[0].id.clone();
        let mut detached = Clip::blank("c2", track, ClipKind::Audio, "Voice", 9.0, 3.0);
        detached.detached_from = Some("c1".into());
        detached.media_id = "m1".into();
        project.active_mut().clips.push(Arc::new(detached));
        assert!(matches!(
            TakeSlot::capture(&project, "c1"),
            Err(TakeError::SeparateAudio)
        ));
    }
}
