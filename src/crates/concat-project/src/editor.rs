// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! The editing session: a project, its id mint, and undo.
//!
//! Undo is whole-state snapshots, and they are cheap: the project's
//! timelines and clips sit behind `Arc`, so a snapshot copies pointers and
//! a command copies only the clip it writes to. Impossible to get wrong in
//! the way inverse operations are, and a heavy project's two hundred undos
//! are a few megabytes, not a few hundred.
//!
//! Two refinements on "one snapshot per command":
//!
//! - A **gesture** is one undo step. A knob being dragged commits on every
//!   pause of the pointer; the caller names the gesture, and commits that
//!   name the same gesture as the last recorded step fold into it. See
//!   [`Editor::apply_within`].
//! - **View state is not an edit.** Which timeline tab is showing, and the
//!   order of the tabs, is saved with the document but never recorded in
//!   history: undo steps back through what changed the picture, not
//!   through what the person was looking at.

use crate::model::AppliedFilter;
use std::collections::VecDeque;

use serde_json::Value;

use crate::commands::{Command, CommandError, IdMint, Outcome, apply};
use crate::doc::{DocumentSettings, from_document, to_document};
use crate::model::{Clip, Project};

const UNDO_DEPTH: usize = 200;

/// The state before one recorded step, and the gesture it belonged to.
struct Snapshot {
    before: Project,
    /// Set while the gesture that produced this step may still continue;
    /// cleared by [`Editor::end_gesture`] or by any step of another kind.
    gesture: Option<String>,
}

/// One editing session: the project, the id mint that keeps its ids unique,
/// and the undo/redo stacks. This is the object a host holds per open
/// project; everything else in the crate is reachable through it.
pub struct Editor {
    project: Project,
    mint: IdMint,
    /// Oldest snapshot at the front: overflowing the depth cap evicts from
    /// the front in constant time, where a Vec would shuffle the whole stack.
    undo: VecDeque<Snapshot>,
    redo: Vec<Project>,
}

impl Editor {
    /// A fresh, empty project.
    pub fn new() -> Self {
        Self::with_video(crate::model::VideoSettings::default())
    }

    /// A fresh editor whose first timeline renders to `video`.
    pub fn with_video(video: crate::model::VideoSettings) -> Self {
        Self {
            project: Project::with_video(video),
            mint: IdMint::default(),
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }

    /// Restores a project from a document, adopting every id it uses so the
    /// mint can never re-issue one. Returns None when the document holds
    /// nothing recognisable.
    pub fn from_document(document: &Value) -> Option<Self> {
        let project = from_document(document)?;
        let mut mint = IdMint::default();
        mint.adopt_project(&project);
        Some(Self {
            project,
            mint,
            undo: VecDeque::new(),
            redo: Vec::new(),
        })
    }

    /// Rewrites the picture links `upgrade` changes - one to a retired
    /// effect, read as a link to the effect that stands in for it - as the
    /// document is read, before any edit: not a step to undo, since it is
    /// what the document means now. Returns whether any link changed.
    pub fn upgrade_links(&mut self, mut upgrade: impl FnMut(&mut AppliedFilter) -> bool) -> bool {
        use std::sync::Arc;
        let mut changed = false;
        for timeline in &mut self.project.timelines {
            for index in 0..timeline.clips.len() {
                let mut links = timeline.clips[index].video_effects.clone();
                let mut any = false;
                for link in &mut links {
                    any |= upgrade(link);
                }
                if any {
                    let timeline = Arc::make_mut(timeline);
                    Arc::make_mut(&mut timeline.clips[index]).video_effects = links;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Takes off the clip fields earlier versions wrote and this one no
    /// longer reads - 0.2.4's `reverse` flag and its preset animations -
    /// as the document is read, the way [`Editor::upgrade_links`] does: not
    /// a step to undo. They rode along unread, so a reversed clip played
    /// forwards and an animated one sat still with nothing said (audit
    /// 2026-09-28, #5). Returns the names of the clips that had each, so
    /// the open can say which; the next save makes it so.
    pub fn drop_retired_fields(&mut self) -> RetiredFields {
        use std::sync::Arc;
        const ANIMATIONS: [&str; 4] = [
            "animationIn",
            "animationOut",
            "animationCombo",
            "animationLoop",
        ];
        let mut found = RetiredFields::default();
        for timeline in &mut self.project.timelines {
            for index in 0..timeline.clips.len() {
                let extra = &timeline.clips[index].extra;
                let reversed = extra.get("reverse").and_then(|v| v.as_bool()) == Some(true);
                let animated = ANIMATIONS
                    .iter()
                    .any(|key| extra.get(*key).is_some_and(|value| !value.is_null()));
                let any = extra.contains_key("reverse")
                    || ANIMATIONS.iter().any(|key| extra.contains_key(*key));
                if !any {
                    continue;
                }
                let name = timeline.clips[index].name.clone();
                if reversed && !found.reversed.contains(&name) {
                    found.reversed.push(name.clone());
                }
                if animated && !found.animated.contains(&name) {
                    found.animated.push(name);
                }
                let timeline = Arc::make_mut(timeline);
                let clip = Arc::make_mut(&mut timeline.clips[index]);
                clip.extra.remove("reverse");
                for key in ANIMATIONS {
                    clip.extra.remove(key);
                }
            }
        }
        found
    }

    /// The current state, read-only: all mutation goes through
    /// [`Editor::apply`] so nothing can change without being undoable.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// The project, writable around the undo stack: for tests that need a
    /// state no command makes any more, such as the stacked clips an older
    /// document can hold.
    #[cfg(test)]
    pub(crate) fn project_mut(&mut self) -> &mut Project {
        &mut self.project
    }

    /// Applies one command, recording the state before it for undo.
    ///
    /// A command that fails leaves the project and history untouched, and
    /// the snapshot is only kept when the command reports it actually
    /// changed something ([`Outcome::applied`]), so undo never replays a
    /// no-op - a missing id, a value already in place.
    pub fn apply(&mut self, command: Command) -> Result<Outcome, CommandError> {
        self.apply_within(None, command)
    }

    /// [`Editor::apply`] as one move of a gesture.
    ///
    /// The first command naming a gesture records a step as usual; every
    /// later command naming the same gesture, with no other step recorded
    /// in between, folds into that step, so undo lands where the gesture
    /// began. Commands that set absolute values (every inspector knob) are
    /// the ones this is for. `None` is a step of its own, and ends any
    /// gesture in progress.
    pub fn apply_within(
        &mut self,
        gesture: Option<&str>,
        command: Command,
    ) -> Result<Outcome, CommandError> {
        let before = (!command.is_view_state()).then(|| self.project.clone());
        let outcome =
            Self::apply_to_project(&mut self.project, &mut self.mint, command, before.as_ref())?;
        if let Some(before) = before
            && outcome.applied
        {
            self.finish_edit(before, gesture);
        }
        Ok(outcome)
    }

    /// Applies a command after checking its normalized result against the
    /// live project. A refusal leaves the project, mint and history intact.
    pub fn apply_checked(
        &mut self,
        command: Command,
        check: impl FnOnce(&Project, &Project) -> Result<(), String>,
    ) -> Result<Outcome, String> {
        let mut project = self.project.clone();
        let mut mint = self.mint.clone();
        let is_edit = !command.is_view_state();
        let outcome = Self::apply_to_project(
            &mut project,
            &mut mint,
            command,
            is_edit.then_some(&self.project),
        )
        .map_err(|error| error.to_string())?;
        check(&self.project, &project)?;

        let before = std::mem::replace(&mut self.project, project);
        self.mint = mint;
        if is_edit && outcome.applied {
            self.finish_edit(before, None);
        }
        Ok(outcome)
    }

    fn apply_to_project(
        project: &mut Project,
        mint: &mut IdMint,
        command: Command,
        before: Option<&Project>,
    ) -> Result<Outcome, CommandError> {
        let outcome = apply(project, mint, command)?;
        if let Some(before) = before
            && outcome.applied
        {
            Self::tidy_touched(project, before);
            Self::follow_first_hdr(project, before);
        }
        Ok(outcome)
    }

    fn finish_edit(&mut self, before: Project, gesture: Option<&str>) {
        let continues = match (gesture, self.undo.back()) {
            (Some(gesture), Some(last)) => last.gesture.as_deref() == Some(gesture),
            _ => false,
        };
        if !continues {
            self.record(before, gesture);
        }
        self.redo.clear();
    }

    fn record(&mut self, before: Project, gesture: Option<&str>) {
        self.undo.push_back(Snapshot {
            before,
            gesture: gesture.map(str::to_owned),
        });
        if self.undo.len() > UNDO_DEPTH {
            self.undo.pop_front();
        }
    }

    /// Restores a project captured from this editor as one new history step.
    /// The mint retains the ids used by the edit being replaced.
    pub fn restore_project(&mut self, project: Project) {
        if project != self.project {
            self.mint.adopt_project(&project);
            let before = std::mem::replace(&mut self.project, project);
            self.record(before, None);
            self.redo.clear();
        }
    }

    /// Runs [`Clip::tidy`] over every clip the command wrote, so the clamps
    /// live in one place and a document is the same on screen as it is
    /// after a save and a reopen (audit 2026-09-23, #13). Only a clip
    /// whose `Arc` the command replaced is looked at: the rest are still
    /// the snapshot's, and were tidy already.
    fn tidy_touched(project: &mut Project, before: &Project) {
        use std::collections::HashSet;
        use std::sync::Arc;
        let previous: std::collections::HashMap<&str, &Arc<crate::model::Timeline>> = before
            .timelines
            .iter()
            .map(|timeline| (timeline.id.as_str(), timeline))
            .collect();
        for timeline in &mut project.timelines {
            let old = previous.get(timeline.id.as_str()).copied();
            if old.is_some_and(|old| Arc::ptr_eq(old, timeline)) {
                continue;
            }
            let kept: HashSet<*const Clip> = old
                .map(|old| old.clips.iter().map(Arc::as_ptr).collect())
                .unwrap_or_default();
            let timeline = Arc::make_mut(timeline);
            for clip in &mut timeline.clips {
                if kept.contains(&Arc::as_ptr(clip)) {
                    continue;
                }
                let tidied = (**clip).clone().tidy();
                if tidied != **clip {
                    *clip = Arc::new(tidied);
                }
            }
        }
    }

    /// Makes an SDR timeline HLG when the command put its first HDR clip on
    /// it - a clip of a file recorded in HLG or PQ, where before it had
    /// none - in the same step, so an undo takes both back. A timeline set
    /// back to SDR with HDR clips already on it stays SDR (decided 26 Sept:
    /// the Modify sheet can change it back), and only a timeline the
    /// command wrote, and did not make, is looked at.
    fn follow_first_hdr(project: &mut Project, before: &Project) {
        use std::collections::HashSet;
        use std::sync::Arc;
        let hdr = |project: &Project| -> HashSet<String> {
            project
                .media
                .iter()
                .filter(|media| media.color_space.is_hdr())
                .map(|media| media.id.clone())
                .collect()
        };
        let now = hdr(project);
        if now.is_empty() {
            return;
        }
        let then = hdr(before);
        let previous: std::collections::HashMap<&str, &Arc<crate::model::Timeline>> = before
            .timelines
            .iter()
            .map(|timeline| (timeline.id.as_str(), timeline))
            .collect();
        for timeline in &mut project.timelines {
            if timeline.video.color_space.is_hdr() {
                continue;
            }
            // A timeline the command made keeps the colour it was made
            // with - a copy of one set back to SDR stays SDR.
            let Some(old) = previous.get(timeline.id.as_str()).copied() else {
                continue;
            };
            if Arc::ptr_eq(old, timeline) {
                continue;
            }
            let had = old.clips.iter().any(|clip| then.contains(&clip.media_id));
            let has = timeline
                .clips
                .iter()
                .any(|clip| now.contains(&clip.media_id));
            if has && !had {
                Arc::make_mut(timeline).video.color_space = crate::model::ColorSpace::Hlg;
            }
        }
    }

    /// Ends the gesture in progress, if any: the next command naming it
    /// starts a new step rather than folding into the last one. Called when
    /// the pointer goes up, or when enough time has passed that a second
    /// drag of the same knob is a second edit.
    pub fn end_gesture(&mut self) {
        if let Some(last) = self.undo.back_mut() {
            last.gesture = None;
        }
    }

    /// Steps back one edit. Returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        match self.undo.pop_back() {
            Some(previous) => {
                self.redo
                    .push(std::mem::replace(&mut self.project, previous.before));
                true
            }
            None => false,
        }
    }

    /// Steps forward again. Returns whether anything changed.
    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                let before = std::mem::replace(&mut self.project, next);
                self.undo.push_back(Snapshot {
                    before,
                    gesture: None,
                });
                true
            }
            None => false,
        }
    }

    /// Whether [`Editor::undo`] would do anything - what the UI's undo
    /// button greys out on.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether [`Editor::redo`] would do anything; the redo button's twin
    /// of [`Editor::can_undo`].
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// The full document for saving.
    pub fn to_document(&self, settings: &DocumentSettings) -> Value {
        to_document(settings, &self.project)
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// The clips [`Editor::drop_retired_fields`] found, by name, each once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetiredFields {
    /// Clips an earlier version played backwards, which now play forwards.
    pub reversed: Vec<String>,
    /// Clips an earlier version moved with a preset animation.
    pub animated: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColorSpace, MediaItem, MediaKind};

    #[test]
    fn refusing_a_checked_batch_preserves_project_mint_and_history() {
        let mut editor = Editor::new();
        let first = editor.project.active_timeline_id.clone();
        editor.apply(Command::AddTimeline).unwrap();
        editor.apply(Command::AddTrack { bottom: false }).unwrap();
        assert!(editor.undo());
        let before = editor.project.clone();
        let redo = editor.redo.clone();
        let steps = editor.undo.len();
        let next = editor.mint.clone().next("t");

        let command = Command::Batch {
            commands: vec![
                Command::AddTrack { bottom: false },
                Command::SelectTimeline {
                    timeline_id: first.clone(),
                },
            ],
        };
        let result = editor.apply_checked(command, |original, proposed| {
            assert_eq!(original, &before);
            assert_eq!(proposed.active_timeline_id, first);
            assert!(
                proposed
                    .timelines
                    .iter()
                    .any(|timeline| { timeline.tracks.iter().any(|track| track.id == next) })
            );
            Err("outside the chosen tracks".into())
        });

        assert_eq!(result, Err("outside the chosen tracks".into()));
        assert_eq!(editor.project(), &before);
        assert_eq!(editor.undo.len(), steps);
        assert_eq!(editor.redo, redo);
        let accepted = editor
            .apply_checked(Command::AddTrack { bottom: false }, |_, _| Ok(()))
            .unwrap();
        assert_eq!(accepted.created_id.as_deref(), Some(next.as_str()));
        assert_eq!(editor.undo.len(), steps + 1);
        assert!(!editor.can_redo());
        assert!(editor.undo());
        assert_eq!(editor.project(), &before);
    }

    #[test]
    fn checked_edit_validates_implicit_hdr_change_before_recording_it() {
        let mut editor = Editor::new();
        editor.project_mut().media.push(MediaItem {
            id: "hdr".into(),
            duration: Some(2.0),
            kind: MediaKind::Video,
            color_space: ColorSpace::Hlg,
            ..Default::default()
        });
        let before = editor.project.clone();
        let command = Command::AddClip {
            media_id: "hdr".into(),
            track_id: before.active().tracks[0].id.clone(),
            start: 0.0,
            ripple: false,
        };
        let check = |original: &Project, proposed: &Project| {
            assert_eq!(original, &before);
            assert_eq!(proposed.active().video.color_space, ColorSpace::Hlg);
            assert_eq!(proposed.active().clips.len(), 1);
        };

        let result = editor.apply_checked(command.clone(), |original, proposed| {
            check(original, proposed);
            Err("keep the timeline in SDR".into())
        });
        assert_eq!(result, Err("keep the timeline in SDR".into()));
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());

        editor
            .apply_checked(command, |original, proposed| {
                check(original, proposed);
                Ok(())
            })
            .unwrap();
        assert_eq!(editor.project().active().video.color_space, ColorSpace::Hlg);
        assert!(editor.undo());
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }

    #[test]
    fn checked_view_state_change_keeps_existing_history() {
        let mut editor = Editor::new();
        let first = editor.project.active_timeline_id.clone();
        editor.apply(Command::AddTimeline).unwrap();
        editor.apply(Command::AddTrack { bottom: false }).unwrap();
        assert!(editor.undo());
        let before = editor.project.clone();
        let steps = editor.undo.len();
        let redo = editor.redo.clone();
        let command = Command::SelectTimeline {
            timeline_id: first.clone(),
        };

        let result = editor.apply_checked(command.clone(), |_, proposed| {
            assert_eq!(proposed.active_timeline_id, first);
            Err("keep the current timeline".into())
        });
        assert_eq!(result, Err("keep the current timeline".into()));
        assert_eq!(editor.project(), &before);

        editor
            .apply_checked(command, |_, proposed| {
                assert_eq!(proposed.active_timeline_id, first);
                Ok(())
            })
            .unwrap();
        assert_eq!(editor.project().active_timeline_id, first);
        assert_eq!(editor.undo.len(), steps);
        assert_eq!(editor.redo, redo);
    }
}
