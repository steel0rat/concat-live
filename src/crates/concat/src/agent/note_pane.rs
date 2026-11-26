// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Timeline notes in the live editor and reports from their owning chat turn.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};
use slint::{ComponentHandle, Model, SharedString, VecModel};

use super::notes::{Note, NoteAnchor, NoteStatus, TimelineNotes};
use super::{AgentPane, Submission, agent_client_id, with_pane};
use crate::format::frames_timecode;
use crate::host::{self, Shell};
use crate::studio::Studio;
use crate::ui::{Agent, TimelineNoteData};

#[derive(Default)]
pub(super) struct NotePane {
    folder: Option<PathBuf>,
    timeline: Option<String>,
    store: Option<TimelineNotes>,
    rows: Rc<VecModel<TimelineNoteData>>,
    drafts: HashMap<PathBuf, SharedString>,
    selected: HashSet<u32>,
    batch: Option<NoteBatch>,
    sequence: u64,
    open: bool,
    load_error: String,
    error: String,
}

struct NoteBatch {
    id: String,
    epoch: u64,
    sent: Vec<Note>,
    reported: HashSet<u32>,
}

impl NoteBatch {
    fn report(&mut self, ids: &[u32]) -> Result<(), String> {
        if ids.len() > self.sent.len()
            || !ids
                .iter()
                .all(|id| self.sent.iter().any(|note| note.id == *id))
        {
            return Err("Report only the note IDs supplied in this batch.".into());
        }
        self.reported.extend(ids.iter().copied());
        Ok(())
    }
}

pub(crate) fn stage_note_report(
    studio: &Studio,
    client: Option<&str>,
    batch_id: &str,
    ids: &[u32],
) -> Result<(), String> {
    let mut result =
        Err("This report does not belong to the current timeline notes request.".into());
    let folder = studio
        .session
        .as_ref()
        .map(|session| Path::new(session.path()));
    with_pane(|pane| {
        if folder.is_some()
            && pane.notes.folder.as_deref() == folder
            && client == Some(agent_client_id(pane.generation).as_str())
            && let Some(batch) = &mut pane.notes.batch
            && batch.epoch == host::project_epoch()
            && batch.id == batch_id
        {
            result = batch.report(ids);
        }
    });
    result
}

fn current_anchor(pane: &AgentPane, studio: &Studio) -> NoteAnchor {
    if let Some([start, end]) = pane.range.as_ref().and_then(|range| range.bounds(studio))
        && end > start
    {
        return NoteAnchor::Range { start, end };
    }
    let playhead = f64::from(studio.playhead).max(0.0);
    if let [selected] = studio.selection.as_slice()
        && let Some(clip) = studio.clip(selected)
    {
        let offset = if (clip.start..clip.start + clip.duration).contains(&playhead) {
            playhead - clip.start
        } else {
            0.0
        };
        return NoteAnchor::Clip {
            clip_id: clip.id.clone(),
            offset,
            fallback: clip.start + offset,
        };
    }
    NoteAnchor::Point { time: playhead }
}

fn anchor_label(anchor: &NoteAnchor, studio: &Studio) -> String {
    let [start, end] = anchor.resolve(studio.timeline());
    let rate = studio.frame_rate();
    let from = frames_timecode(start as f32, rate);
    match anchor {
        NoteAnchor::Range { .. } => format!("{from}–{}", frames_timecode(end as f32, rate)),
        NoteAnchor::Clip { clip_id, .. } => {
            let name = studio
                .clip(clip_id)
                .map_or("Missing clip", |clip| clip.name.as_str());
            format!("{from} · {name}")
        }
        NoteAnchor::Point { .. } => from,
    }
}

fn wire_note(note: &Note, studio: &Studio) -> Value {
    let [start, end] = note.anchor.resolve(studio.timeline());
    let mut data = json!({
        "id": note.id,
        "text": note.text,
        "startSeconds": start,
        "endSeconds": end,
    });
    if let NoteAnchor::Clip { clip_id, .. } = &note.anchor
        && studio.clip(clip_id).is_some()
    {
        data["clipId"] = json!(clip_id);
    }
    data
}

impl NotePane {
    fn notes(&self) -> &[Note] {
        self.store.as_ref().map_or(&[], TimelineNotes::notes)
    }

    fn visible(&self) -> impl Iterator<Item = &Note> {
        self.notes()
            .iter()
            .filter(|note| Some(note.timeline_id.as_str()) == self.timeline.as_deref())
    }
}

impl AgentPane {
    pub(super) fn install_notes_model(&self) {
        if let Some(app) = self.ui.upgrade() {
            app.global::<Agent>()
                .set_notes(self.notes.rows.clone().into());
        }
    }

    pub(super) fn sync_notes(&mut self, studio: &Studio) {
        let folder = studio
            .session
            .as_ref()
            .map(|session| PathBuf::from(session.path()));
        let timeline = studio
            .session
            .as_ref()
            .map(|_| studio.timeline().id.as_str());
        if self.notes.folder != folder {
            self.switch_note_project(folder);
        }
        if self.notes.timeline.as_deref() != timeline {
            self.notes.timeline = timeline.map(str::to_owned);
        }
        self.publish_note_rows(studio);
    }

    fn switch_note_project(&mut self, folder: Option<PathBuf>) {
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            if let Some(previous) = self.notes.folder.take() {
                let text = global.get_note_draft();
                if !text.trim().is_empty() {
                    self.notes.drafts.insert(previous, text);
                }
            }
            let restored = folder
                .as_ref()
                .and_then(|next| self.notes.drafts.remove(next));
            global.set_note_draft(restored.unwrap_or_default());
            global.set_note_edit_id(0);
        }
        self.notes.folder = folder;
        self.notes.selected.clear();
        self.notes.batch = None;
        self.notes.error.clear();
        self.notes.load_error.clear();
        self.notes.store = None;
        if let Some(folder) = self.notes.folder.as_deref() {
            match TimelineNotes::load(folder) {
                Ok(store) => self.notes.store = Some(store),
                Err(error) => self.notes.load_error = error,
            }
        } else {
            self.notes.open = false;
        }
    }

    pub(super) fn publish_note_target(&self, studio: &Studio) {
        if let Some(app) = self.ui.upgrade() {
            let label = studio
                .session
                .as_ref()
                .map(|_| anchor_label(&current_anchor(self, studio), studio));
            app.global::<Agent>()
                .set_note_target(label.unwrap_or_default().into());
        }
    }

    pub(super) fn publish_note_controls(&self, global: &Agent) {
        global.set_notes_open(self.notes.open);
        let error = if self.notes.error.is_empty() {
            &self.notes.load_error
        } else {
            &self.notes.error
        };
        global.set_notes_error(error.as_str().into());
        global.set_notes_can_send(
            self.notes.batch.is_none()
                && self.can_submit()
                && self.notes.visible().any(|note| {
                    note.status == NoteStatus::Open && self.notes.selected.contains(&note.id)
                }),
        );
    }

    pub(super) fn notes_have_draft(&self) -> bool {
        !self.notes.drafts.is_empty()
            || self.ui.upgrade().is_some_and(|app| {
                let global = app.global::<Agent>();
                !global.get_note_draft().trim().is_empty()
                    || self.notes.notes().iter().any(|note| {
                        note.id as i32 == global.get_note_edit_id()
                            && note.text != global.get_note_edit_draft().as_str()
                    })
            })
    }

    pub(super) fn toggle_notes(&mut self) {
        if self.notes.open && self.enabled() && !self.maintenance {
            self.close_notes_view();
            self.publish();
        } else {
            self.open_notes_view();
        }
    }

    fn open_notes_view(&mut self) -> bool {
        if self.notes.folder.is_none()
            || !self.enabled()
            || self.maintenance
            || self.dictation.active()
        {
            return false;
        }
        self.notes.open = true;
        self.close_queue();
        self.close_edit_scope();
        self.close_brief_view();
        self.history.open = false;
        super::forms::reveal_agent_pane();
        self.publish();
        true
    }

    pub(super) fn close_notes_view(&mut self) {
        self.notes.open = false;
    }

    pub(super) fn open_note_from_shortcut(&mut self) -> bool {
        if !self.open_notes_view() {
            return false;
        }
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            global.set_note_focus(global.get_note_focus().wrapping_add(1));
        }
        true
    }

    fn publish_note_rows(&self, studio: &Studio) {
        let mut rows: Vec<_> = self
            .notes
            .visible()
            .map(|note| {
                let [start, end] = note.anchor.resolve(studio.timeline());
                TimelineNoteData {
                    id: note.id as i32,
                    start: start as f32,
                    end: end as f32,
                    label: anchor_label(&note.anchor, studio).into(),
                    text: note.text.as_str().into(),
                    status: match note.status {
                        NoteStatus::Open => 0,
                        NoteStatus::Review => 1,
                        NoteStatus::Done => 2,
                    },
                    selected: self.notes.selected.contains(&note.id),
                    working: self
                        .notes
                        .batch
                        .as_ref()
                        .is_some_and(|batch| batch.sent.contains(note)),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.id.cmp(&b.id)));
        let model = &self.notes.rows;
        if model.row_count() != rows.len()
            || rows
                .iter()
                .enumerate()
                .any(|(index, row)| model.row_data(index).is_none_or(|old| old.id != row.id))
        {
            model.set_vec(rows);
        } else {
            for (index, row) in rows.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            }
        }
    }

    fn with_note_studio(&self, mut read: impl FnMut(&Studio)) {
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow()
                && studio.session.as_ref().is_some_and(|session| {
                    Some(Path::new(session.path())) == self.notes.folder.as_deref()
                })
            {
                read(&studio);
            }
        });
    }

    fn refresh_notes(&self) {
        self.with_note_studio(|studio| self.publish_note_rows(studio));
        self.publish();
    }

    fn change_notes(
        &mut self,
        edit: impl FnOnce(&mut TimelineNotes) -> Result<(), String>,
    ) -> bool {
        if !self.enabled() || self.maintenance {
            return false;
        }
        let result = self
            .notes
            .store
            .as_mut()
            .ok_or_else(|| self.notes.load_error.clone())
            .and_then(edit);
        let success = result.is_ok();
        self.notes.error = result.err().unwrap_or_default();
        self.refresh_notes();
        success
    }

    pub(super) fn add_note(&mut self, text: &str) -> bool {
        let mut captured = None;
        self.with_note_studio(|studio| {
            captured = Some((studio.timeline().id.clone(), current_anchor(self, studio)));
        });
        let Some((timeline, anchor)) = captured else {
            return false;
        };
        self.change_notes(|store| store.add(&timeline, anchor, text).map(|_| ()))
    }

    pub(super) fn edit_note(&mut self, id: i32, text: &str) -> bool {
        self.change_notes(|store| store.edit(id as u32, text))
    }

    pub(super) fn review_note(&mut self, id: i32, accepted: bool) {
        if self.change_notes(|store| store.review(id as u32, accepted)) && accepted {
            self.notes.selected.remove(&(id as u32));
            self.refresh_notes();
        }
    }

    pub(super) fn remove_note(&mut self, id: i32) {
        if self.change_notes(|store| store.remove(id as u32)) {
            self.notes.selected.remove(&(id as u32));
        }
    }

    pub(super) fn select_note(&mut self, id: i32, selected: bool) {
        let id = id as u32;
        if !self
            .notes
            .visible()
            .any(|note| note.id == id && note.status == NoteStatus::Open)
        {
            return;
        }
        if selected {
            self.notes.selected.insert(id);
        } else {
            self.notes.selected.remove(&id);
        }
        self.refresh_notes();
    }

    pub(super) fn jump_to_note(&self, id: i32) {
        host::on_ui(move |studio, _, _| {
            let mut target = None;
            with_pane(|pane| {
                if studio.session.as_ref().is_some_and(|session| {
                    Some(Path::new(session.path())) == pane.notes.folder.as_deref()
                }) && let Some(note) = pane.notes.visible().find(|note| note.id == id as u32)
                    && note.timeline_id == studio.timeline().id
                {
                    target = Some(note.anchor.resolve(studio.timeline())[0] as f32);
                }
            });
            if let Some(target) = target {
                studio.seek(target);
            }
        });
    }

    pub(super) fn send_notes(&mut self) {
        if !self.can_submit() || self.notes.batch.is_some() {
            return;
        }
        let sent: Vec<_> = self
            .notes
            .visible()
            .filter(|note| {
                note.status == NoteStatus::Open && self.notes.selected.contains(&note.id)
            })
            .cloned()
            .collect();
        if sent.is_empty() {
            return;
        }
        let mut captured = None;
        self.with_note_studio(|studio| {
            if sent
                .iter()
                .all(|note| note.timeline_id == studio.timeline().id)
            {
                let payload: Vec<_> = sent.iter().map(|note| wire_note(note, studio)).collect();
                captured = Some((host::project_epoch(), studio.timeline().id.clone(), payload));
            }
        });
        let Some((epoch, timeline, payload)) = captured else {
            return;
        };
        self.notes.sequence += 1;
        let id = format!(
            "{}-{}",
            agent_client_id(self.generation),
            self.notes.sequence
        );
        let detail = format!(
            " Selected timeline notes: {}. Timeline notes batch ID: {id}. Timeline ID: {}. Work through the selected notes in the visible project. Respect their positions and clip targets and report only completed notes using timeline_notes_report with this batchId. The user will review them in Concat.",
            json!(payload),
            json!(timeline),
        );
        let message = if sent.len() == 1 {
            "Work on 1 timeline note.".to_owned()
        } else {
            format!("Work on {} timeline notes.", sent.len())
        };
        if self.send_prompt(message, None, Submission::QuickAction, detail) {
            self.notes.batch = Some(NoteBatch {
                id,
                epoch,
                sent,
                reported: HashSet::new(),
            });
            self.notes.open = false;
            self.notes.error.clear();
            self.refresh_notes();
        } else {
            self.notes.error.clone_from(&self.error);
            self.publish();
        }
    }

    pub(super) fn cancel_note_batch(&mut self) {
        if self.notes.batch.take().is_some() {
            self.refresh_notes();
        }
    }

    pub(super) fn finish_note_batch(&mut self, failed: bool) {
        let Some(batch) = self.notes.batch.take() else {
            return;
        };
        if !failed
            && batch.epoch == host::project_epoch()
            && let Some(store) = &mut self.notes.store
        {
            let total = batch.sent.len();
            let reported: Vec<_> = batch
                .sent
                .into_iter()
                .filter(|note| batch.reported.contains(&note.id))
                .collect();
            match store.mark_for_review(&reported) {
                Ok(reviewed) => {
                    self.notes.error.clear();
                    self.notes.selected.retain(|id| {
                        store
                            .notes()
                            .iter()
                            .any(|note| note.id == *id && note.status == NoteStatus::Open)
                    });
                    self.local_message(
                        "Context",
                        format!("{reviewed} of {total} notes ready for review."),
                    );
                }
                Err(error) => {
                    self.notes.error = error;
                    self.error.clone_from(&self.notes.error);
                }
            }
        }
        self.refresh_notes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_reports_only_its_own_notes_and_retries_keep_the_same_selection() {
        use super::super::storage::Scratch;

        let root = Scratch::new();
        let mut store = TimelineNotes::load(root.path()).unwrap();
        let id = store
            .add("main", NoteAnchor::Point { time: 1.0 }, "Move title")
            .unwrap();
        let mut batch = NoteBatch {
            id: "qa-batch".into(),
            epoch: 1,
            sent: store.notes().to_vec(),
            reported: HashSet::new(),
        };
        assert!(batch.report(&[id, id + 1]).is_err());
        assert!(batch.report(&[id + 1]).is_err());
        assert!(batch.reported.is_empty());
        batch.report(&[id]).unwrap();
        batch.report(&[id]).unwrap();
        assert_eq!(batch.reported, HashSet::from([id]));
    }
}
