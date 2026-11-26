// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Project notes live beside the edit document, independently of its undo history.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use concat_project::model::{Timeline, ranges::MAX_TIME};
use serde::{Deserialize, Serialize};

use super::storage::{read_json, write_json};

const FILE: &str = "concat-notes.json";
const VERSION: u32 = 1;
const MAX_NOTES: usize = 4096;
const MAX_NOTE_CHARS: usize = 4096;
const MAX_ID: u32 = i32::MAX as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NoteStatus {
    Open,
    Review,
    Done,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(super) enum NoteAnchor {
    Point {
        time: f64,
    },
    Range {
        start: f64,
        end: f64,
    },
    Clip {
        clip_id: String,
        offset: f64,
        fallback: f64,
    },
}

impl NoteAnchor {
    pub(super) fn resolve(&self, timeline: &Timeline) -> [f64; 2] {
        match self {
            Self::Point { time } => [*time, *time],
            Self::Range { start, end } => [*start, *end],
            Self::Clip {
                clip_id,
                offset,
                fallback,
            } => {
                let time = timeline
                    .clip(clip_id)
                    .map_or(*fallback, |clip| clip.start + offset.min(clip.duration));
                [time, time]
            }
        }
    }

    fn valid(&self) -> bool {
        // Both the start and duration of a clip can reach MAX_TIME.
        let time_valid = |time| (0.0..=2.0 * MAX_TIME).contains(&time);
        match self {
            Self::Point { time } => time_valid(*time),
            Self::Range { start, end } => time_valid(*start) && time_valid(*end) && start < end,
            Self::Clip {
                clip_id,
                offset,
                fallback,
            } => !clip_id.is_empty() && (0.0..=MAX_TIME).contains(offset) && time_valid(*fallback),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Note {
    pub id: u32,
    pub timeline_id: String,
    pub anchor: NoteAnchor,
    pub text: String,
    pub status: NoteStatus,
    // A report must not advance a note edited and then restored to the same text.
    revision: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotesDocument {
    version: u32,
    revision: u64,
    last_id: u32,
    notes: Vec<Note>,
}

impl Default for NotesDocument {
    fn default() -> Self {
        Self {
            version: VERSION,
            revision: 0,
            last_id: 0,
            notes: Vec::new(),
        }
    }
}

impl NotesDocument {
    fn note_mut(&mut self, id: u32) -> Result<&mut Note, String> {
        self.notes
            .iter_mut()
            .find(|note| note.id == id)
            .ok_or_else(|| "This timeline note no longer exists.".to_owned())
    }

    fn valid(&self) -> bool {
        self.last_id <= MAX_ID
            && u64::from(self.last_id) <= self.revision
            && self.notes.len() <= MAX_NOTES
            && self.notes.windows(2).all(|pair| pair[0].id < pair[1].id)
            && self.notes.iter().all(|note| {
                note.id != 0
                    && note.id <= self.last_id
                    && note.revision != 0
                    && note.revision <= self.revision
                    && !note.timeline_id.is_empty()
                    && note.anchor.valid()
                    && validated_text(&note.text).is_ok()
            })
    }
}

pub(super) struct TimelineNotes {
    file: PathBuf,
    stored: NotesDocument,
}

impl TimelineNotes {
    pub(super) fn load(folder: &Path) -> Result<Self, String> {
        let file = folder.join(FILE);
        let stored = Self::read_document(&file)?;
        Ok(Self { file, stored })
    }

    pub(super) fn notes(&self) -> &[Note] {
        &self.stored.notes
    }

    pub(super) fn add(
        &mut self,
        timeline_id: &str,
        anchor: NoteAnchor,
        text: &str,
    ) -> Result<u32, String> {
        let text = validated_text(text)?;
        if timeline_id.is_empty() || !anchor.valid() {
            return Err("Choose a valid timeline position for the note.".into());
        }

        self.update(|doc, revision| {
            if doc.notes.len() >= MAX_NOTES {
                return Err(format!(
                    "A project can contain up to {MAX_NOTES} timeline notes."
                ));
            }
            let id = doc
                .last_id
                .checked_add(1)
                .filter(|id| *id <= MAX_ID)
                .ok_or_else(|| "No more timeline note IDs are available.".to_owned())?;
            doc.last_id = id;
            doc.notes.push(Note {
                id,
                timeline_id: timeline_id.to_owned(),
                anchor,
                text: text.to_owned(),
                status: NoteStatus::Open,
                revision,
            });
            Ok(id)
        })
    }

    pub(super) fn edit(&mut self, id: u32, text: &str) -> Result<(), String> {
        let text = validated_text(text)?;
        self.update(|doc, revision| {
            let note = doc.note_mut(id)?;
            if note.text != text {
                note.text = text.to_owned();
                note.status = NoteStatus::Open;
                note.revision = revision;
            }
            Ok(())
        })
    }

    pub(super) fn remove(&mut self, id: u32) -> Result<(), String> {
        self.update(|doc, _| {
            doc.notes.retain(|note| note.id != id);
            Ok(())
        })
    }

    pub(super) fn review(&mut self, id: u32, accepted: bool) -> Result<(), String> {
        self.update(|doc, revision| {
            let note = doc.note_mut(id)?;
            if accepted && note.status == NoteStatus::Open {
                return Err("Only notes awaiting review can be approved.".into());
            }
            let status = if accepted {
                NoteStatus::Done
            } else {
                NoteStatus::Open
            };
            if note.status != status {
                note.status = status;
                note.revision = revision;
            }
            Ok(())
        })
    }

    pub(super) fn mark_for_review(&mut self, notes: &[Note]) -> Result<usize, String> {
        if notes.is_empty() {
            return Ok(0);
        }
        self.update(|doc, revision| {
            let mut count = 0;
            for submitted in notes {
                if let Ok(note) = doc.note_mut(submitted.id)
                    && note.status == NoteStatus::Open
                    && &*note == submitted
                {
                    note.status = NoteStatus::Review;
                    note.revision = revision;
                    count += 1;
                }
            }
            Ok(count)
        })
    }

    fn read_document(file: &Path) -> Result<NotesDocument, String> {
        let document: NotesDocument = read_json(file)
            .map_err(|error| format!("Could not read timeline notes. {error}"))?
            .unwrap_or_default();
        if document.version != VERSION {
            return Err(format!(
                "Unsupported timeline notes format in {}.",
                file.display()
            ));
        }
        if !document.valid() {
            return Err(format!("Invalid timeline notes in {}.", file.display()));
        }
        Ok(document)
    }

    fn update<R>(
        &mut self,
        edit: impl FnOnce(&mut NotesDocument, u64) -> Result<R, String>,
    ) -> Result<R, String> {
        let _lock = self.lock()?;
        let mut next = Self::read_document(&self.file)?;
        if next != self.stored {
            return Err(
                "Timeline notes changed on disk. Reopen the project to reload them.".into(),
            );
        }
        let revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| "No more timeline note revisions are available.".to_owned())?;
        let result = edit(&mut next, revision)?;
        if next != self.stored {
            next.revision = revision;
            // Retain the decoded representation for the next compare, including JSON float rounding.
            let persisted = serde_json::to_vec(&next)
                .and_then(|bytes| serde_json::from_slice(&bytes))
                .map_err(|error| format!("Could not prepare timeline notes. {error}"))?;
            write_json(&self.file, &next)?;
            self.stored = persisted;
        }
        Ok(result)
    }

    fn lock(&self) -> Result<File, String> {
        // A lock on the replaced JSON inode would not guard the next write.
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(self.file.with_file_name(".concat-notes.lock"))
            .map_err(|error| format!("Could not open the timeline notes lock. {error}"))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => {
                Err("Another window is changing these timeline notes. Try again.".into())
            }
            Err(TryLockError::Error(error)) => {
                Err(format!("Could not lock timeline notes. {error}"))
            }
        }
    }
}

fn validated_text(text: &str) -> Result<&str, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Write some text for the timeline note.".into());
    }
    if text.chars().take(MAX_NOTE_CHARS + 1).count() > MAX_NOTE_CHARS {
        return Err(format!(
            "A timeline note can contain up to {MAX_NOTE_CHARS} characters."
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use concat_project::model::{Clip, ClipKind};

    use super::super::storage::Scratch;
    use super::*;

    #[test]
    fn notes_reopen_with_their_timeline_anchor_status_and_non_reused_id() {
        let folder = Scratch::new();
        let mut notes = TimelineNotes::load(folder.path()).unwrap();
        let point = notes
            .add(
                "TL1",
                NoteAnchor::Point {
                    time: 0.100_000_000_000_000_02,
                },
                " \nПоправить титр\nВторая строка\t",
            )
            .unwrap();
        let anchor = NoteAnchor::Range {
            start: 7.0,
            end: 12.0,
        };
        let other = notes.add("TL2", anchor.clone(), "Leave this cut").unwrap();
        let snapshot = notes.notes()[1].clone();
        assert_eq!(notes.mark_for_review(&[snapshot]).unwrap(), 1);
        notes.review(other, true).unwrap();
        notes.remove(point).unwrap();

        let mut reopened = TimelineNotes::load(folder.path()).unwrap();
        assert_eq!(reopened.notes(), notes.notes());
        assert_eq!(reopened.notes()[0].timeline_id, "TL2");
        assert_eq!(reopened.notes()[0].anchor, anchor);
        assert_eq!(reopened.notes()[0].status, NoteStatus::Done);
        reopened.review(other, false).unwrap();
        assert_eq!(reopened.notes()[0].status, NoteStatus::Open);
        let next = reopened
            .add("TL1", NoteAnchor::Point { time: 4.0 }, "New note")
            .unwrap();
        assert!(next > other);
    }

    #[test]
    fn a_batch_only_reports_the_unchanged_open_notes() {
        let folder = Scratch::new();
        let mut notes = TimelineNotes::load(folder.path()).unwrap();
        let point = NoteAnchor::Point { time: 1.0 };
        let edited = notes.add("TL1", point.clone(), "Colour").unwrap();
        let pending = notes.add("TL1", point, "Audio").unwrap();
        let original = notes.notes().to_vec();

        notes.edit(edited, "Warm colour").unwrap();
        notes.edit(edited, "Colour").unwrap();
        assert_eq!(notes.mark_for_review(&original).unwrap(), 1);
        assert_eq!(notes.notes()[0].status, NoteStatus::Open);
        assert_eq!(notes.notes()[1].status, NoteStatus::Review);
        assert_eq!(notes.mark_for_review(&original).unwrap(), 0);

        notes.review(pending, true).unwrap();
        notes.edit(pending, "Lower the music").unwrap();
        assert_eq!(notes.notes()[1].status, NoteStatus::Open);
    }

    #[test]
    fn stale_and_damaged_documents_are_left_on_disk() {
        let folder = Scratch::new();
        let mut notes = TimelineNotes::load(folder.path()).unwrap();
        let mut stale = TimelineNotes::load(folder.path()).unwrap();
        let point = NoteAnchor::Point { time: 1.0 };
        let id = notes.add("TL1", point.clone(), "First note").unwrap();
        assert!(stale.add("TL1", point, "Stale write").is_err());
        assert!(stale.notes().is_empty());
        assert_eq!(
            TimelineNotes::load(folder.path()).unwrap().notes(),
            notes.notes()
        );

        let path = folder.path().join(FILE);
        for invalid in [
            b"unfinished" as &[u8],
            br#"{"version":2,"revision":0,"lastId":0,"notes":[]}"#,
            br#"{"version":1,"revision":0,"lastId":0,"notes":[],"unknown":true}"#,
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(TimelineNotes::load(folder.path()).is_err());
            assert!(notes.edit(id, "Do not overwrite").is_err());
            assert_eq!(fs::read(&path).unwrap(), invalid);
            assert_eq!(notes.notes()[0].text, "First note");
        }
    }

    #[test]
    fn clip_notes_follow_the_clip_and_fall_back_to_the_original_time() {
        let anchor = NoteAnchor::Clip {
            clip_id: "c1".into(),
            offset: 2.0,
            fallback: 9.0,
        };
        let mut timeline = Timeline::default();
        timeline.clips.push(Arc::new(Clip::blank(
            "c1",
            "T1",
            ClipKind::Video,
            "Footage",
            7.0,
            4.0,
        )));
        assert_eq!(anchor.resolve(&timeline), [9.0, 9.0]);
        timeline.clip_mut("c1").unwrap().start = 20.0;
        assert_eq!(anchor.resolve(&timeline), [22.0, 22.0]);
        timeline.clip_mut("c1").unwrap().duration = 1.0;
        assert_eq!(anchor.resolve(&timeline), [21.0, 21.0]);
        timeline.clips.clear();
        assert_eq!(anchor.resolve(&timeline), [9.0, 9.0]);
    }
}
