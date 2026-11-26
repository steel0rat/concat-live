// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Removes pauses from clip slots using the editor's normal split operation.

use std::collections::{BTreeMap, HashSet};

use super::{ClipPause, Command, CommandError, IdMint, MIN_CLIP_DURATION, Outcome, clips};
use crate::model::{Clip, ClipKind, Project, Timeline};

type ClipWindows<'a> = BTreeMap<&'a str, Vec<(f64, f64)>>;

const LINK_EPSILON: f64 = 1e-6;

fn refuse(reason: &'static str) -> CommandError {
    CommandError::CannotTrimPauses { reason }
}

/// Checks whether all the proposed cuts can land together on the given timeline.
pub fn validate_clip_pauses(
    timeline: &Timeline,
    cuts: &[ClipPause],
    scope_end: Option<f64>,
) -> Result<(), CommandError> {
    checked_windows(timeline, cuts, scope_end).map(|_| ())
}

fn checked_windows<'a>(
    timeline: &Timeline,
    cuts: &'a [ClipPause],
    scope_end: Option<f64>,
) -> Result<ClipWindows<'a>, CommandError> {
    if scope_end.is_some_and(|end| !end.is_finite()) {
        return Err(CommandError::NotANumber);
    }
    let mut windows: ClipWindows = BTreeMap::new();
    for cut in cuts {
        if !cut.start.is_finite() || !cut.end.is_finite() {
            return Err(CommandError::NotANumber);
        }
        windows
            .entry(&cut.clip_id)
            .or_default()
            .push((cut.start, cut.end));
    }
    for intervals in windows.values_mut() {
        intervals.sort_by(|left, right| left.0.total_cmp(&right.0));
    }
    for (&id, intervals) in &windows {
        let clip = timeline
            .clip(id)
            .ok_or_else(|| refuse("A clip for the pause cuts is no longer on this timeline."))?;
        if !matches!(clip.kind, ClipKind::Audio | ClipKind::Video) {
            return Err(refuse("Pause cuts need an audio or video clip."));
        }
        if timeline.track(&clip.track_id).is_none() {
            return Err(CommandError::TrackGone);
        }
        // The editor's split windows transform and effect keys. Preset
        // motion and animated masks would instead restart on every piece.
        if clip
            .motion
            .as_ref()
            .is_some_and(|motion| motion.engine().is_some())
            || clip
                .masks
                .iter()
                .any(|mask| mask.keys.values().any(|keys| !keys.is_empty()))
        {
            return Err(refuse(
                "Pause cuts would restart a clip animation or animated mask.",
            ));
        }
        let end = clip.start + clip.duration;
        let cut_until = scope_end.map_or(end, |out| out.min(end));
        check_edges(clip, intervals, cut_until)?;
        check_transitions(timeline, clip, intervals[0].0, cut_until)?;
        check_detached_audio(timeline, clip, intervals, &windows)?;
    }
    Ok(windows)
}

fn check_edges(clip: &Clip, intervals: &[(f64, f64)], cut_until: f64) -> Result<(), CommandError> {
    let end_of_clip = clip.start + clip.duration;
    if cut_until < end_of_clip && end_of_clip - cut_until <= MIN_CLIP_DURATION {
        return Err(refuse(
            "The Out point is too close to the clip end to keep its tail.",
        ));
    }
    if cut_until < end_of_clip && cut_until > end_of_clip - clip.fade_out {
        return Err(refuse("The Out point is inside a clip fade."));
    }
    let mut previous = clip.start;
    for &(start, end) in intervals {
        if start - previous <= MIN_CLIP_DURATION
            || end - start <= MIN_CLIP_DURATION
            || cut_until - end <= MIN_CLIP_DURATION
        {
            return Err(refuse(
                "Pause cuts must be separated and leave room between the cut and each clip edge.",
            ));
        }
        if start < clip.start + clip.fade_in || end > end_of_clip - clip.fade_out {
            return Err(refuse("A pause cut would overlap a clip fade."));
        }
        previous = end;
    }
    Ok(())
}

fn check_transitions(
    timeline: &Timeline,
    clip: &Clip,
    first_cut: f64,
    cut_until: f64,
) -> Result<(), CommandError> {
    if clip.kind != ClipKind::Video {
        return Ok(());
    }
    let lane = timeline
        .tracks
        .iter()
        .position(|track| track.id == clip.track_id);
    let Some(lane) = lane.filter(|&index| timeline.tracks[index].visible) else {
        return Ok(());
    };
    let frame = 1.0 / timeline.video.rate();
    let half_frame = frame / 2.0;
    let end = clip.start + clip.duration;
    let same_visible_lane = |other: &&Clip| {
        other.id != clip.id && other.track_id == clip.track_id && other.kind.is_visual()
    };
    let visible_clips = timeline.clips.iter().map(std::sync::Arc::as_ref);
    let incoming = clip.transition_in.as_ref().is_some_and(|transition| {
        visible_clips
            .clone()
            .filter(same_visible_lane)
            .any(|other| {
                let length = transition.duration.min(clip.duration).min(other.duration);
                (other.start + other.duration - clip.start).abs() < half_frame
                    && length >= frame
                    && first_cut - clip.start < length
            })
    });
    let outgoing = visible_clips
        .clone()
        .filter(same_visible_lane)
        .any(|other| {
            other.transition_in.as_ref().is_some_and(|transition| {
                let length = transition.duration.min(clip.duration).min(other.duration);
                (other.start - end).abs() < half_frame
                    && length >= frame
                    && end - cut_until < length
            })
        });
    let placed = visible_clips.clone().any(|other| {
        other.kind == ClipKind::Transition
            && other.start - half_frame < cut_until
            && other.start + other.duration + half_frame >= first_cut
            && timeline.tracks[lane + 1..]
                .iter()
                .any(|track| track.visible && track.id == other.track_id)
    });
    if incoming || outgoing || placed {
        return Err(refuse("A pause cut would change a transition."));
    }
    Ok(())
}

fn check_detached_audio(
    timeline: &Timeline,
    clip: &Clip,
    intervals: &[(f64, f64)],
    windows: &ClipWindows<'_>,
) -> Result<(), CommandError> {
    let paired = |other: &Clip| {
        (clip.start - other.start).abs() <= LINK_EPSILON
            && (clip.duration - other.duration).abs() <= LINK_EPSILON
            && windows
                .get(other.id.as_str())
                .is_some_and(|cuts| cuts == intervals)
    };
    let parent_aligned = clip.detached_from.as_deref().is_none_or(|id| {
        timeline
            .clip(id)
            .is_some_and(|parent| parent.kind == ClipKind::Video && paired(parent))
    });
    let children_aligned = timeline
        .clips
        .iter()
        .filter(|other| other.detached_from.as_deref() == Some(&clip.id))
        .all(|child| child.kind == ClipKind::Audio && paired(child));
    if parent_aligned && children_aligned {
        Ok(())
    } else {
        Err(refuse(
            "Cut aligned detached audio and its picture together at the same times.",
        ))
    }
}

pub(super) fn apply(
    project: &mut Project,
    mint: &mut IdMint,
    timeline_id: &str,
    cuts: &[ClipPause],
    scope_end: Option<f64>,
) -> Result<Outcome, CommandError> {
    if project.active().id != timeline_id {
        return Err(refuse(
            "The active timeline changed before the pause cuts could be applied.",
        ));
    }
    let windows = checked_windows(project.active(), cuts, scope_end)?;
    if windows.is_empty() {
        return Ok(Outcome::default());
    }

    let mut staged = project.clone();
    let mut next_ids = mint.clone();
    let mut pieces = BTreeMap::new();
    let mut removed = HashSet::new();
    for (&id, intervals) in &windows {
        let survivors = cut_clip(
            &mut staged,
            &mut next_ids,
            id,
            intervals,
            scope_end,
            &mut removed,
        )?;
        pieces.insert(id, survivors);
    }
    let timeline = staged.active_mut();
    timeline.clips.retain(|clip| !removed.contains(&clip.id));
    for (id, survivors) in &pieces {
        if let Some(picture) = project
            .active()
            .clip(id)
            .and_then(|clip| clip.detached_from.as_deref())
        {
            // Each aligned group used the same intervals, so the surviving
            // sound pieces follow their corresponding picture pieces.
            let picture_pieces = &pieces[picture];
            for (sound, picture) in survivors.iter().zip(picture_pieces).skip(1) {
                let sound = timeline.clip_mut(sound).ok_or_else(missing_split)?;
                sound.detached_from = Some(picture.clone());
            }
        }
    }

    *project = staged;
    *mint = next_ids;
    Ok(Outcome {
        created_id: pieces
            .values()
            .next_back()
            .and_then(|group| group.get(1))
            .cloned(),
        applied: true,
    })
}

fn cut_clip(
    project: &mut Project,
    mint: &mut IdMint,
    id: &str,
    intervals: &[(f64, f64)],
    scope_end: Option<f64>,
    removed: &mut HashSet<String>,
) -> Result<Vec<String>, CommandError> {
    let clip = project.active().clip(id).ok_or_else(missing_split)?;
    let end = clip.start + clip.duration;
    let outside = scope_end
        .filter(|out| *out < end)
        .map(|out| split(project, mint, id, out))
        .transpose()?;
    let mut tails = Vec::with_capacity(intervals.len());
    // Work right to left so the original id still names the uncut head
    // at each boundary and all times still refer to the original clip.
    for &(start, end) in intervals.iter().rev() {
        tails.push(split(project, mint, id, end)?);
        removed.insert(split(project, mint, id, start)?);
    }
    let timeline = project.active_mut();
    let mut removed_duration = 0.0;
    for ((start, end), tail) in intervals.iter().zip(tails.iter().rev()) {
        removed_duration += end - start;
        let clip = timeline.clip_mut(tail).ok_or_else(missing_split)?;
        clip.start = end - removed_duration;
    }
    Ok(std::iter::once(id.to_owned())
        .chain(tails.into_iter().rev())
        .chain(outside)
        .collect())
}

fn missing_split() -> CommandError {
    refuse("A clip could not be split at the proposed pause boundary.")
}

fn split(
    project: &mut Project,
    mint: &mut IdMint,
    clip_id: &str,
    time: f64,
) -> Result<String, CommandError> {
    clips::apply(
        project,
        mint,
        Command::SplitClips {
            clip_ids: vec![clip_id.to_owned()],
            time,
        },
    )?
    .created_id
    .ok_or_else(missing_split)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::{ClipMotion, ClipMove, SpeedPoint, Transition};
    use crate::{Editor, speed};

    fn clip(id: &str, track: &str, kind: ClipKind, start: f64, duration: f64) -> Clip {
        Clip::blank(id, track, kind, id, start, duration)
    }

    fn add(editor: &mut Editor, clip: Clip) {
        editor.project_mut().active_mut().clips.push(Arc::new(clip));
    }

    fn pause(id: &str, start: f64, end: f64) -> ClipPause {
        ClipPause {
            clip_id: id.into(),
            start,
            end,
        }
    }

    fn trim(editor: &mut Editor, cuts: Vec<ClipPause>) -> Result<Outcome, CommandError> {
        editor.apply(Command::TrimClipPauses {
            timeline_id: "TL1".into(),
            cuts,
            scope_end: None,
        })
    }

    #[test]
    fn pause_cuts_pack_each_target_in_its_slot_in_one_undo_step() {
        let mut editor = Editor::new();
        let mut picture = clip("picture", "T1", ClipKind::Video, 2.0, 10.0);
        picture.source_start = 5.0;
        picture.speed = 1.5;
        add(&mut editor, picture);
        let mut sound = clip("sound", "T2", ClipKind::Audio, 0.0, 8.0);
        sound.speed_curve = Some(vec![
            SpeedPoint {
                at: 0.0,
                speed: 0.5,
                slope: None,
            },
            SpeedPoint {
                at: 1.0,
                speed: 1.5,
                slope: None,
            },
        ]);
        let sound_source_at_tail = speed::source_consumed(&sound, 3.0);
        add(&mut editor, sound);
        add(&mut editor, clip("next", "T1", ClipKind::Video, 12.0, 2.0));
        add(&mut editor, clip("title", "T3", ClipKind::Text, 6.0, 8.0));
        let before = editor.project().clone();

        let outcome = trim(
            &mut editor,
            vec![
                pause("picture", 8.0, 10.0),
                pause("sound", 1.0, 3.0),
                pause("picture", 5.0, 6.0),
            ],
        )
        .unwrap();
        assert!(outcome.applied);
        let timeline = editor.project().active();
        let picture: Vec<_> = timeline
            .clips
            .iter()
            .filter(|clip| clip.name == "picture")
            .map(|clip| (clip.start, clip.duration, clip.source_start))
            .collect();
        assert_eq!(
            picture,
            [(2.0, 3.0, 5.0), (5.0, 2.0, 11.0), (7.0, 2.0, 17.0)]
        );
        let sound_tail = timeline
            .clips
            .iter()
            .find(|clip| clip.name == "sound" && clip.id != "sound")
            .unwrap();
        assert_eq!((sound_tail.start, sound_tail.duration), (1.0, 5.0));
        assert!((sound_tail.source_start - sound_source_at_tail).abs() < 1e-6);
        for name in ["next", "title"] {
            assert_eq!(timeline.clip(name), before.active().clip(name));
        }
        assert!(timeline.clip(&outcome.created_id.unwrap()).is_some());
        let after = editor.project().clone();
        assert!(editor.undo());
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
        assert!(editor.redo());
        assert_eq!(editor.project(), &after);
    }

    #[test]
    fn a_marked_out_keeps_the_same_clip_suffix_at_its_original_time() {
        let mut editor = Editor::new();
        let mut picture = clip("picture", "T1", ClipKind::Video, 2.0, 10.0);
        picture.speed = 1.5;
        picture.source_start = 10.0;
        add(&mut editor, picture);
        let before = editor.project().clone();

        let cuts = vec![pause("picture", 5.0, 6.0)];
        assert!(validate_clip_pauses(before.active(), &cuts, Some(11.99)).is_err());
        assert!(validate_clip_pauses(before.active(), &cuts, Some(5.5)).is_err());
        assert!(validate_clip_pauses(before.active(), &cuts, Some(f64::NAN)).is_err());
        editor
            .apply(Command::TrimClipPauses {
                timeline_id: "TL1".into(),
                cuts,
                scope_end: Some(9.0),
            })
            .unwrap();
        let parts: Vec<_> = editor
            .project()
            .active()
            .clips
            .iter()
            .map(|clip| (clip.start, clip.duration, clip.source_start))
            .collect();
        assert_eq!(
            parts,
            [(2.0, 3.0, 10.0), (5.0, 3.0, 16.0), (9.0, 3.0, 20.5)]
        );
        assert!(editor.undo());
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }

    #[test]
    fn clips_whose_animation_would_restart_are_refused() {
        let mut editor = Editor::new();
        let mut picture = clip("picture", "T1", ClipKind::Video, 0.0, 8.0);
        picture.motion = Some(ClipMotion {
            enter: Some(ClipMove {
                kind: "fade".into(),
                seconds: 0.75,
            }),
            ..ClipMotion::default()
        });
        add(&mut editor, picture);
        let before = editor.project().clone();
        let cuts = vec![pause("picture", 2.0, 3.0)];
        assert!(validate_clip_pauses(before.active(), &cuts, Some(5.0)).is_err());
        assert!(trim(&mut editor, cuts).is_err());
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }

    #[test]
    fn invalid_cuts_and_stale_timeline_are_refused_atomically() {
        let mut editor = Editor::new();
        add(&mut editor, clip("first", "T1", ClipKind::Audio, 0.0, 8.0));
        add(&mut editor, clip("second", "T2", ClipKind::Audio, 9.0, 5.0));
        let before = editor.project().clone();

        for bad in [
            pause("second", 8.9, 10.0),
            pause("second", 11.0, 11.0),
            pause("second", 11.0, f64::NAN),
            pause("absent", 11.0, 12.0),
            pause("first", 2.0, 4.0),
            pause("first", 3.0, 5.0),
        ] {
            assert!(trim(&mut editor, vec![pause("first", 2.0, 3.0), bad]).is_err());
            assert_eq!(editor.project(), &before);
            assert!(!editor.can_undo());
        }
        let stale = editor.apply(Command::TrimClipPauses {
            timeline_id: "old-timeline".into(),
            cuts: vec![pause("first", 2.0, 3.0)],
            scope_end: None,
        });
        assert!(stale.is_err());
        assert_eq!(editor.project(), &before);
    }

    #[test]
    fn aligned_detached_sound_stays_linked_to_its_corresponding_picture() {
        let mut editor = Editor::new();
        let mut picture = clip("picture", "T1", ClipKind::Video, 0.0, 8.0);
        picture.muted = Some(true);
        add(&mut editor, picture);
        let mut sound = clip("sound", "T2", ClipKind::Audio, 0.0, 8.0);
        sound.detached_from = Some("picture".into());
        add(&mut editor, sound);
        let before = editor.project().clone();
        assert!(trim(&mut editor, vec![pause("sound", 2.0, 3.0)]).is_err());
        assert_eq!(editor.project(), &before);
        trim(
            &mut editor,
            vec![pause("picture", 2.0, 3.0), pause("sound", 2.0, 3.0)],
        )
        .unwrap();
        let timeline = editor.project().active();
        let sounds: Vec<_> = timeline
            .clips
            .iter()
            .filter(|clip| clip.kind == ClipKind::Audio)
            .collect();
        assert_eq!(sounds.len(), 2);
        for sound in sounds {
            let picture = timeline
                .clip(sound.detached_from.as_deref().unwrap())
                .unwrap();
            assert_eq!(picture.kind, ClipKind::Video);
            assert_eq!(
                (picture.start, picture.duration),
                (sound.start, sound.duration)
            );
        }
    }

    #[test]
    fn an_incoming_transition_survives_a_late_cut_and_other_transitions_are_protected() {
        let mut editor = Editor::new();
        add(&mut editor, clip("lead", "T1", ClipKind::Video, 0.0, 3.0));
        let mut main = clip("main", "T1", ClipKind::Video, 3.0, 8.0);
        main.transition_in = Some(Transition {
            id: "cross-fade".into(),
            duration: 1.0,
            params: BTreeMap::new(),
        });
        add(&mut editor, main);
        let before = editor.project().clone();
        assert!(validate_clip_pauses(before.active(), &[pause("main", 3.5, 4.0)], None).is_err());
        assert!(trim(&mut editor, vec![pause("lead", 1.0, 2.0)]).is_err());
        assert_eq!(editor.project(), &before);
        add(
            &mut editor,
            clip("overlay", "T2", ClipKind::Transition, 9.0, 1.0),
        );
        assert!(trim(&mut editor, vec![pause("main", 5.0, 6.0)]).is_err());
        editor
            .project_mut()
            .active_mut()
            .clips
            .retain(|clip| clip.id != "overlay");
        trim(&mut editor, vec![pause("main", 5.0, 6.0)]).unwrap();
        let timeline = editor.project().active();
        assert_eq!(
            timeline.clip("main").unwrap().transition_in,
            before.active().clip("main").unwrap().transition_in
        );
        let tail = timeline
            .clips
            .iter()
            .find(|clip| clip.name == "main" && clip.id != "main")
            .unwrap();
        assert!(tail.transition_in.is_none());
    }
}
