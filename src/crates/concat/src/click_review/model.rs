// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::collections::BTreeMap;

use concat_project::commands::{ClipPatch, Command};
use concat_project::model::{Clip, ClipKind, Project};
use concat_project::placement::TOUCH;

const FADE: f64 = 0.005;
const MIN_STEP: f64 = 1_800.0;
const SEAM_SLACK: usize = 2;
const CONTEXT: usize = 32;

#[derive(Clone, PartialEq, Debug)]
pub(super) struct Join {
    pub at: f64,
    pub track: usize,
    pub track_id: String,
    pub left_id: String,
    pub right_id: String,
}

pub(super) struct Review {
    original: Project,
    pub joins: Vec<Join>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FadeError {
    ProjectChanged,
    TrackLocked,
    UnknownJoin,
    NothingToChange,
}

impl Review {
    pub fn capture(project: &Project, locked: impl Fn(&str) -> bool) -> Self {
        let timeline = project.active();
        let has_audio = |clip: &Clip| {
            matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
                && clip.muted != Some(true)
                && project.media_by_id(&clip.media_id).is_some_and(|media| {
                    media.has_audio && !media.placeholder && !media.path.is_empty()
                })
        };
        let mut joins = Vec::new();
        for (index, track) in timeline.tracks.iter().enumerate() {
            if track.muted || locked(&track.id) {
                continue;
            }
            let mut clips: Vec<_> = timeline
                .clips
                .iter()
                .filter(|clip| clip.track_id == track.id && has_audio(clip))
                .collect();
            clips.sort_by(|a, b| a.start.total_cmp(&b.start));
            for pair in clips.windows(2) {
                let (left, right) = (pair[0], pair[1]);
                if (left.start + left.duration - right.start).abs() > TOUCH
                    || (left.fade_out >= wanted_fade(left) && right.fade_in >= wanted_fade(right))
                {
                    continue;
                }
                joins.push(Join {
                    at: right.start,
                    track: index,
                    track_id: track.id.clone(),
                    left_id: left.id.clone(),
                    right_id: right.id.clone(),
                });
            }
        }
        joins.sort_by(|a, b| a.at.total_cmp(&b.at).then(a.track.cmp(&b.track)));
        Self {
            original: project.clone(),
            joins,
        }
    }

    pub fn is_current(&self, project: &Project) -> bool {
        self.original == *project
    }

    pub fn fades(
        &self,
        project: &Project,
        chosen: &[usize],
        locked: impl Fn(&str) -> bool,
    ) -> Result<Command, FadeError> {
        if !self.is_current(project) {
            return Err(FadeError::ProjectChanged);
        }
        let timeline = project.active();
        let mut edits: BTreeMap<String, ClipPatch> = BTreeMap::new();
        for &index in chosen {
            let join = self.joins.get(index).ok_or(FadeError::UnknownJoin)?;
            if locked(&join.track_id) {
                return Err(FadeError::TrackLocked);
            }
            let left = timeline.clip(&join.left_id).ok_or(FadeError::UnknownJoin)?;
            let right = timeline
                .clip(&join.right_id)
                .ok_or(FadeError::UnknownJoin)?;
            let tail = wanted_fade(left);
            if tail > left.fade_out {
                edits.entry(left.id.clone()).or_default().fade_out = Some(tail);
            }
            let head = wanted_fade(right);
            if head > right.fade_in {
                edits.entry(right.id.clone()).or_default().fade_in = Some(head);
            }
        }
        if edits.is_empty() {
            return Err(FadeError::NothingToChange);
        }
        Ok(Command::Batch {
            commands: edits
                .into_iter()
                .map(|(clip_id, patch)| Command::UpdateClip { clip_id, patch })
                .collect(),
        })
    }
}

fn wanted_fade(clip: &Clip) -> f64 {
    FADE.min(clip.duration / 2.0)
}

/// The largest unusually abrupt step within two samples of the cut, in
/// i16 sample units. Measure each channel against its own nearby motion.
pub(super) fn discontinuity(stereo: &[[i16; 2]], cut: usize) -> Option<u32> {
    if cut < CONTEXT + SEAM_SLACK + 1 || stereo.len().saturating_sub(cut) < CONTEXT + SEAM_SLACK + 1
    {
        return None;
    }
    let first = cut - SEAM_SLACK;
    let last = cut + SEAM_SLACK;
    (0..2)
        .filter_map(|channel| {
            let step = |i: usize| i16::abs_diff(stereo[i][channel], stereo[i - 1][channel]);
            let nearby = (first - CONTEXT..first).chain(last + 1..=last + CONTEXT);
            let energy: f64 = nearby.map(|i| f64::from(step(i)).powi(2)).sum();
            let normal = (energy / (2 * CONTEXT) as f64).sqrt();
            let jump = (first..=last).map(step).max()?;
            (f64::from(jump) >= MIN_STEP.max(normal * 4.0 + 300.0)).then_some(u32::from(jump))
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::model::{MediaItem, MediaKind};
    use concat_project::{DocumentSettings, Editor};
    use std::sync::Arc;

    fn tone(frames: usize, phase_step: bool) -> Vec<[i16; 2]> {
        (0..frames)
            .map(|i| {
                let theta = (i as f64 / 48_000.0) * 440.0 * std::f64::consts::TAU;
                let signal = (theta.cos() * 11_000.0) as i16;
                [
                    signal,
                    if phase_step && i >= frames / 2 {
                        -signal
                    } else {
                        signal
                    },
                ]
            })
            .collect()
    }

    #[test]
    fn pcm_step_in_one_channel_is_found_at_the_join() {
        let stereo = tone(4_800, true);
        assert!(discontinuity(&stereo, 2_400).unwrap() > 20_000);
        assert_eq!(discontinuity(&tone(4_800, false), 2_400), None);
        assert_eq!(discontinuity(&[[0, 0]; 100], 50), None);
    }

    fn project() -> Project {
        let mut project = Project::default();
        project.media.push(MediaItem {
            id: "m1".into(),
            path: "/qa/tone.wav".into(),
            kind: MediaKind::Audio,
            duration: Some(12.0),
            has_audio: true,
            ..Default::default()
        });
        let timeline = project.active_mut();
        let track_id = timeline.tracks[0].id.clone();
        for (index, start) in [0.0, 1.0, 2.0, 6.0].into_iter().enumerate() {
            let id = format!("c{}", index + 1);
            let mut clip = Clip::blank(id, &track_id, ClipKind::Audio, "Tone", start, 1.0);
            clip.media_id = "m1".into();
            timeline.clips.push(Arc::new(clip));
        }
        Arc::make_mut(&mut timeline.clips[0]).fade_out = 0.01;
        project
    }

    #[test]
    fn selected_edits_preserve_long_fades_and_are_one_undo_step() {
        let settings = DocumentSettings {
            name: "Clicks".to_owned(),
            width: 320,
            height: 180,
            rate_num: 30,
            rate_den: 1,
            canvas: [0, 0, 0, 255],
        };
        let saved = concat_project::to_document(&settings, &project());
        let mut editor = Editor::from_document(&saved).unwrap();
        let before = editor.to_document(&settings);
        let review = Review::capture(editor.project(), |_| false);
        assert_eq!(
            review.joins.iter().map(|join| join.at).collect::<Vec<_>>(),
            [1.0, 2.0]
        );
        let edit = review.fades(editor.project(), &[0, 1], |_| false).unwrap();
        editor.apply(edit).unwrap();
        let timeline = editor.project().active();
        assert_eq!(timeline.clip("c1").unwrap().fade_out, 0.01);
        assert_eq!(timeline.clip("c2").unwrap().fade_in, FADE);
        assert_eq!(timeline.clip("c2").unwrap().fade_out, FADE);
        assert_eq!(timeline.clip("c3").unwrap().fade_in, FADE);
        assert_eq!(timeline.clip("c4").unwrap().fade_in, 0.0);
        assert!(editor.undo());
        assert_eq!(editor.to_document(&settings), before);
        assert!(!editor.can_undo());
    }

    #[test]
    fn changing_either_clip_or_lock_refuses_the_plan() {
        let mut project = project();
        let review = Review::capture(&project, |_| false);
        assert_eq!(
            review.fades(&project, &[0], |_| true),
            Err(FadeError::TrackLocked)
        );
        Arc::make_mut(&mut project.active_mut().clips[1]).source_start = 1.0;
        assert_eq!(
            review.fades(&project, &[0], |_| false),
            Err(FadeError::ProjectChanged)
        );
    }
}
