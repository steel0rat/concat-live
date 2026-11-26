// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Timeline context and checkpoints for edits made by the embedded chat.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use concat_host::projects::{self, ProjectInfo};
use concat_project::model::{Clip, Timeline};
use concat_project::{DocumentSettings, Project};
use serde_json::{Value, json};

use crate::format::frames_timecode;
use crate::host::project_epoch;
use crate::studio::Studio;

pub(super) struct MarkedRange {
    epoch: u64,
    timeline: String,
    start: f32,
    end: Option<f32>,
}

impl MarkedRange {
    pub(super) fn start(studio: &Studio) -> Option<Self> {
        let session = studio.session.as_ref()?;
        Some(Self {
            epoch: project_epoch(),
            timeline: session.project().active_timeline_id.clone(),
            start: studio.playhead,
            end: None,
        })
    }

    pub(super) fn belongs_to(&self, studio: &Studio) -> bool {
        self.epoch == project_epoch()
            && studio
                .session
                .as_ref()
                .is_some_and(|session| self.timeline == session.project().active_timeline_id)
    }

    pub(super) fn bounds(&self, studio: &Studio) -> Option<[f64; 2]> {
        let end = self.end?;
        self.belongs_to(studio).then_some([
            f64::from(self.start.min(end)),
            f64::from(self.start.max(end)),
        ])
    }

    pub(super) fn set_end(&mut self, studio: &Studio) {
        if self.belongs_to(studio) {
            self.end = Some(studio.playhead);
        }
    }
}

#[derive(Clone)]
pub(super) struct TimelineInput {
    timeline: String,
    name: String,
    playhead: f32,
    rate: f32,
    clips: Vec<(String, String, f64, f64)>,
    range: Option<(f64, f64)>,
    waiting_for_end: Option<f32>,
}

impl TimelineInput {
    pub(super) fn capture(studio: &Studio, marked: Option<&MarkedRange>) -> Option<Self> {
        let timeline = studio.session.as_ref()?.project().active();
        let clips: Vec<_> = studio
            .selection
            .iter()
            .filter_map(|id| timeline.clip(id))
            .map(|clip| {
                (
                    clip.id.clone(),
                    clip.name.clone(),
                    clip.start,
                    clip.start + clip.duration,
                )
            })
            .collect();
        let selected_span = clips.iter().fold(None::<(f64, f64)>, |span, clip| {
            Some(span.map_or((clip.2, clip.3), |(start, end)| {
                (start.min(clip.2), end.max(clip.3))
            }))
        });
        let marked = marked.filter(|range| range.belongs_to(studio));
        let range = marked
            .and_then(|range| {
                let end = range.end?;
                Some((
                    f64::from(range.start.min(end)),
                    f64::from(range.start.max(end)),
                ))
            })
            .or(selected_span);
        Some(Self {
            timeline: timeline.id.clone(),
            name: timeline.name.clone(),
            playhead: studio.playhead,
            rate: timeline.video.rate() as f32,
            clips,
            range,
            waiting_for_end: marked
                .filter(|range| range.end.is_none())
                .map(|range| range.start),
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.timeline
    }

    pub(super) fn awaiting_out(&self) -> bool {
        self.waiting_for_end.is_some()
    }

    pub(super) fn has_target(&self) -> bool {
        !self.awaiting_out()
            && (!self.clips.is_empty() || self.range.is_some_and(|(start, end)| end > start))
    }

    pub(super) fn label(&self) -> String {
        if let Some(at) = self.waiting_for_end {
            return format!("In {} · set Out", frames_timecode(at, self.rate));
        }
        if let Some((start, end)) = self.range {
            let times = time_span(start, end, self.rate);
            return match self.clips.as_slice() {
                [] => times,
                [clip] => format!("{times} · {}", clip.1),
                clips => format!("{times} · {} clips", clips.len()),
            };
        }
        format!("Playhead {}", frames_timecode(self.playhead, self.rate))
    }

    pub(super) fn wire(&self) -> Value {
        let clips: Vec<_> = self
            .clips
            .iter()
            .map(|(id, name, start, end)| {
                json!({"id": id, "name": name, "startSeconds": start, "endSeconds": end})
            })
            .collect();
        json!({
            "timelineId": self.timeline,
            "timelineName": self.name,
            "playheadSeconds": self.playhead,
            "rangeSeconds": self.range,
            "selectedClips": clips,
        })
    }
}

fn time_span(start: f64, end: f64, rate: f32) -> String {
    let start = frames_timecode(start as f32, rate);
    let end = frames_timecode(end as f32, rate);
    if start == end {
        start
    } else {
        format!("{start}–{end}")
    }
}

#[derive(Clone)]
pub(crate) struct Checkpoint {
    pub(super) folder: PathBuf,
    pub(super) project: Project,
    pub(super) settings: DocumentSettings,
    epoch: u64,
    revision: u64,
}

impl Checkpoint {
    pub(super) fn capture(studio: &Studio) -> Option<Self> {
        let session = studio.session.as_ref()?;
        Some(Self {
            folder: PathBuf::from(session.path()),
            project: session.project().clone(),
            settings: session.settings(),
            epoch: project_epoch(),
            revision: studio.edit_revision(),
        })
    }

    pub(super) fn save_copy(mut self) -> Result<ProjectInfo, String> {
        let parent = self
            .folder
            .parent()
            .ok_or("The project has no parent folder.")?;
        let stem: String = self.settings.name.chars().take(50).collect();
        let base = format!("{stem} - before Codex");
        let (folder, name) = (1..=100)
            .find_map(|index| {
                let name = if index == 1 {
                    base.clone()
                } else {
                    format!("{base} {index}")
                };
                let folder = parent.join(projects::folder_name(&name));
                match std::fs::create_dir(&folder) {
                    Ok(()) => Some(Ok((folder, name))),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(format!("Could not create a project copy: {error}"))),
                }
            })
            .ok_or("No free name for the project copy.")??;

        // Media already imported into the original project stays there.
        // Point relative paths at that folder so a sibling copy can read it.
        for path in self
            .project
            .media
            .iter_mut()
            .map(|media| &mut media.path)
            .chain(self.project.fonts.iter_mut().map(|font| &mut font.path))
        {
            if !path.is_empty() && Path::new(path).is_relative() {
                *path = self.folder.join(&*path).to_string_lossy().into_owned();
            }
        }
        self.settings.name = name;
        let folder = folder.to_str().ok_or("The copy path is not valid UTF-8.")?;
        let document = concat_project::to_document(&self.settings, &self.project);
        projects::save(folder, &document)?;
        projects::open(folder)
    }
}

#[derive(Clone)]
pub(super) struct EditLink {
    pub(super) timeline: String,
    pub(super) clip: String,
    pub(super) at: f32,
    pub(super) label: String,
}

impl EditLink {
    fn for_clip(timeline: &Timeline, clip: &Clip, deleted: bool) -> Self {
        let times = time_span(
            clip.start,
            clip.start + clip.duration,
            timeline.video.rate() as f32,
        );
        let suffix = if deleted { " (removed)" } else { "" };
        Self {
            timeline: timeline.id.clone(),
            clip: clip.id.clone(),
            at: clip.start as f32,
            label: format!("{times} · {}{suffix}", clip.name),
        }
    }
}

pub(super) struct EditReceipt {
    pub(super) before: Checkpoint,
    pub(super) links: Vec<EditLink>,
    pub(super) clip_count: usize,
    latest_revision: u64,
    interleaved: bool,
}

impl EditReceipt {
    pub(super) fn can_undo(&self, studio: &Studio) -> bool {
        self.can_undo_at(project_epoch(), studio.edit_revision())
    }

    pub(super) fn follow_timeline_switch(&mut self, previous: u64, studio: &Studio) {
        if self.can_undo_at(project_epoch(), previous) {
            self.latest_revision = studio.edit_revision();
        }
    }

    fn can_undo_at(&self, epoch: u64, revision: u64) -> bool {
        !self.interleaved && self.before.epoch == epoch && self.latest_revision == revision
    }
}

#[derive(Default)]
pub(super) struct TurnEdits {
    before: Option<Checkpoint>,
    links: BTreeMap<(String, String), EditLink>,
    latest_revision: u64,
    interleaved: bool,
}

impl TurnEdits {
    pub(super) fn edit(&mut self, before: Checkpoint, studio: &Studio) {
        let Some(after) = studio.session.as_ref() else {
            return;
        };
        if self
            .before
            .as_ref()
            .is_some_and(|base| base.epoch != before.epoch)
            || before.epoch != project_epoch()
        {
            return;
        }
        if before.project == *after.project() && before.settings.name == studio.project_name {
            // A no-op editor command can still advance the Studio revision.
            if self.before.is_some() && self.latest_revision == before.revision {
                self.latest_revision = studio.edit_revision();
            }
            return;
        }
        self.links
            .extend(changed_clips(&before.project, after.project()));
        if self.before.is_some() && self.latest_revision != before.revision {
            self.interleaved = true;
        }
        self.latest_revision = studio.edit_revision();
        self.before.get_or_insert(before);
    }

    pub(super) fn receipt(self) -> Option<EditReceipt> {
        let before = self.before?;
        let clip_count = self.links.len();
        let mut links: Vec<_> = self.links.into_values().collect();
        links.sort_by(|left, right| {
            left.timeline
                .cmp(&right.timeline)
                .then_with(|| left.at.total_cmp(&right.at))
        });
        links.truncate(8);
        Some(EditReceipt {
            before,
            links,
            clip_count,
            latest_revision: self.latest_revision,
            interleaved: self.interleaved,
        })
    }
}

fn changed_clips(before: &Project, after: &Project) -> Vec<((String, String), EditLink)> {
    let mut changes = Vec::new();
    for timeline in &after.timelines {
        let previous = before.timelines.iter().find(|old| old.id == timeline.id);
        if previous.is_some_and(|old| std::sync::Arc::ptr_eq(old, timeline)) {
            continue;
        }
        for clip in &timeline.clips {
            if previous.and_then(|old| old.clip(&clip.id)) != Some(clip.as_ref()) {
                changes.push((
                    (timeline.id.clone(), clip.id.clone()),
                    EditLink::for_clip(timeline, clip, false),
                ));
            }
        }
        if let Some(previous) = previous {
            for clip in &previous.clips {
                if timeline.clip(&clip.id).is_none() {
                    changes.push((
                        (timeline.id.clone(), clip.id.clone()),
                        EditLink::for_clip(timeline, clip, true),
                    ));
                }
            }
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::Editor;

    #[test]
    fn shortcuts_require_a_selected_clip_or_a_complete_nonempty_range() {
        let mut input = TimelineInput {
            timeline: "tl1".into(),
            name: "Timeline".into(),
            playhead: 3.0,
            rate: 24.0,
            clips: Vec::new(),
            range: None,
            waiting_for_end: None,
        };
        assert!(!input.has_target());
        input.range = Some((3.0, 3.0));
        assert!(!input.has_target());
        input.range = Some((2.0, 4.0));
        assert!(input.has_target());
        input.waiting_for_end = Some(2.0);
        assert!(!input.has_target());
        input.waiting_for_end = None;
        input.range = None;
        input.clips.push(("c1".into(), "Shape".into(), 0.0, 2.0));
        assert!(input.has_target());
    }

    #[test]
    fn a_chat_revert_is_guarded_by_project_epoch_and_other_edits() {
        let before = Checkpoint {
            folder: PathBuf::from("/project"),
            project: Project::default(),
            settings: DocumentSettings {
                name: "Project".into(),
                width: 320,
                height: 180,
                rate_num: 24,
                rate_den: 1,
                canvas: [0, 0, 0, 255],
            },
            epoch: 3,
            revision: 10,
        };
        let mut receipt = EditReceipt {
            before,
            links: Vec::new(),
            clip_count: 0,
            latest_revision: 12,
            interleaved: false,
        };
        assert!(receipt.can_undo_at(3, 12));
        assert!(!receipt.can_undo_at(3, 13));
        assert!(!receipt.can_undo_at(4, 12));
        receipt.interleaved = true;
        assert!(!receipt.can_undo_at(3, 12));
    }

    #[test]
    fn restoring_a_chat_checkpoint_leaves_an_editor_history_step() {
        let mut editor = Editor::new();
        let before = editor.project().clone();
        for _ in 0..2 {
            editor
                .apply(concat_project::Command::AddTrack { bottom: false })
                .unwrap();
        }
        let after = editor.project().clone();
        assert_ne!(after, before);
        editor.restore_project(before.clone());
        assert_eq!(editor.project(), &before);
        assert!(editor.undo());
        assert_eq!(editor.project(), &after);
        assert!(editor.redo());
        assert_eq!(editor.project(), &before);
    }
}
