// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use concat_project::Command;
use concat_project::model::{Background, Clip, Project};
use slint::ComponentHandle;

use crate::format::frames_timecode;
use crate::host::{self, Shell};
use crate::i18n::t;
use crate::panes::Msg;
use crate::panes::media_bin::{MEDIA_EXTENSIONS, MediaMsg};
use crate::studio::{Studio, sync};
use crate::ui::{App, IncomingFileData, MediaUseData, MediaWorkflow};

const INCOMING_PREVIEW_PREFIX: &str = "incoming:";

impl Studio {
    pub(crate) fn open_incoming_media(&mut self) {
        if self.session.is_none() {
            return;
        }
        self.close_media_workflow();
        self.media.workflow.inbox_open = true;
        self.scan_incoming_folder();
    }

    pub(crate) fn close_media_workflow(&mut self) {
        let state = &mut self.media.workflow;
        state.inbox_open = false;
        state.uses_id = None;
        state.scan_generation += 1;
        state.preview_generation += 1;
        state.scan_busy = false;
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.card.starts_with(INCOMING_PREVIEW_PREFIX))
        {
            self.close_source();
        }
    }

    pub(crate) fn choose_incoming_folder() {
        Shell::with(|shell, _| {
            let studio = shell.studio.borrow();
            if !studio.media.workflow.inbox_open {
                return;
            }
            let start = studio
                .prefs
                .incoming_media_folder
                .as_deref()
                .map(Path::to_string_lossy)
                .map(|folder| folder.into_owned())
                .unwrap_or_else(|| studio.prefs.project_location().unwrap_or("").to_owned());
            let epoch = host::project_epoch();
            host::after_dialog(
                move || crate::platform::pick_folder(&t("mediaBin.chooseIncomingFolder"), &start),
                move |studio, _, _, folder| {
                    if epoch != host::project_epoch() || !studio.media.workflow.inbox_open {
                        return;
                    }
                    if studio.prefs.incoming_media_folder.as_ref() != Some(&folder) {
                        studio.close_media_workflow();
                        let state = &mut studio.media.workflow;
                        state.inbox_open = true;
                        state.files.clear();
                        state.selected.clear();
                        state.scan_done = false;
                        state.scan_error = false;
                        studio.prefs.incoming_media_folder = Some(folder);
                        studio.prefs.save(&studio.host.dirs);
                    }
                    studio.scan_incoming_folder();
                },
            );
        });
    }

    pub(crate) fn scan_incoming_folder(&mut self) {
        let state = &mut self.media.workflow;
        if self.session.is_none() || !state.inbox_open || state.scan_busy {
            return;
        }
        let Some(folder) = self.prefs.incoming_media_folder.clone() else {
            return;
        };
        state.scan_busy = true;
        let generation = state.scan_generation;
        host::spawn_in_project_publishing(
            move || scan_folder(&folder),
            move |studio, _, _, result| {
                let state = &mut studio.media.workflow;
                if generation != state.scan_generation {
                    return;
                }
                state.scan_busy = false;
                state.scan_done = true;
                state.scan_error = result.is_err();
                match result {
                    Ok(files) => {
                        state
                            .selected
                            .retain(|path| files.binary_search(path).is_ok());
                        state.files = files;
                    }
                    Err(_) => state.files.clear(),
                }
            },
            |studio, app, _| studio.publish_media_workflow(app),
        );
    }

    fn incoming_file_available(&self, path: &Path) -> bool {
        let state = &self.media.workflow;
        state.inbox_open
            && state
                .files
                .binary_search_by(|file| file.as_path().cmp(path))
                .is_ok()
            && !state.pending.contains(path)
            && !self
                .project()
                .media
                .iter()
                .any(|media| Path::new(&media.path) == path)
    }

    pub(crate) fn toggle_incoming_file(&mut self, path: &str) {
        let path = Path::new(path);
        if !self.incoming_file_available(path) {
            return;
        }
        let selected = &mut self.media.workflow.selected;
        if !selected.remove(path) {
            selected.insert(path.to_owned());
        }
    }

    pub(crate) fn preview_incoming_file(&mut self, path: &str) {
        if !self.incoming_file_available(Path::new(path)) {
            return;
        }
        let state = &mut self.media.workflow;
        state.preview_generation += 1;
        let generation = state.preview_generation;
        let card = format!("{INCOMING_PREVIEW_PREFIX}{path}");
        let path = path.to_owned();
        host::spawn_in_project(
            move || concat_host::media::probe(&path),
            move |studio, _, _, result| {
                if studio.media.workflow.preview_generation != generation {
                    return;
                }
                match result {
                    Ok(summary) => {
                        let name = Path::new(&summary.path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(&summary.path)
                            .to_owned();
                        studio.preview_probed_file(&card, &name, summary, false);
                    }
                    Err(error) => studio.notify(&host::probe_error(&error), true),
                }
            },
        );
    }

    pub(crate) fn import_incoming_selection(&mut self) {
        let state = &self.media.workflow;
        let paths: Vec<_> = state
            .files
            .iter()
            .filter(|path| state.selected.contains(*path) && self.incoming_file_available(path))
            .cloned()
            .collect();
        if paths.is_empty() {
            return;
        }
        self.media.workflow.pending.extend(paths.iter().cloned());
        self.handle(Msg::Media(MediaMsg::Import(paths)));
    }

    pub(crate) fn show_selected_media_uses(&mut self) {
        if self.media.selected.len() == 1
            && let Some(id) = self.media.selected.iter().next()
        {
            self.show_media_uses(&id.clone());
        }
    }

    pub(crate) fn show_media_uses(&mut self, id: &str) {
        if self.project().media_by_id(id).is_none() {
            return;
        }
        self.close_media_workflow();
        self.media.workflow.uses_id = Some(id.to_owned());
        self.media.workflow.uses_revision.set(None);
    }

    pub(crate) fn jump_to_media_use(&mut self, timeline_id: &str, clip_id: &str) {
        let Some(media_id) = self.media.workflow.uses_id.as_deref() else {
            return;
        };
        let Some(timeline) = self
            .project()
            .timelines
            .iter()
            .find(|timeline| timeline.id == timeline_id)
        else {
            return;
        };
        let at = if clip_id.is_empty() && shows_image(&timeline.background, media_id) {
            0.0
        } else if let Some(clip) = timeline
            .clips
            .iter()
            .find(|clip| clip.id == clip_id && clip_uses_media(clip, media_id))
        {
            clip.start as f32
        } else {
            return;
        };

        if let Some(first) = self.voiceover.on_seek() {
            if first {
                self.notify(&t("voiceover.playheadHeld"), false);
            }
            return;
        }
        self.pause();
        self.stop_glide();
        if self.timeline().id != timeline_id {
            self.apply(Command::SelectTimeline {
                timeline_id: timeline_id.to_owned(),
            });
            if self.timeline().id != timeline_id {
                return;
            }
        }
        self.transition_selected = None;
        self.selection.clear();
        if !clip_id.is_empty() {
            self.selection.push(clip_id.to_owned());
        }
        self.seek(at);
        let visible_span = self.lanes.width * self.lanes.seconds_per_pixel;
        if !self.lanes.centred
            && visible_span > 0.0
            && (at < self.lanes.scroll_left || at >= self.lanes.scroll_left + visible_span)
        {
            self.lanes.scroll_left = (at - visible_span * 0.1).max(0.0);
        }
    }

    pub(crate) fn publish_media_workflow(&self, app: &App) {
        let ui = app.global::<MediaWorkflow>();
        let state = &self.media.workflow;
        ui.set_inbox_open(state.inbox_open);
        ui.set_can_show_uses(self.media.selected.len() == 1);
        if state.inbox_open {
            let imported: HashSet<_> = self
                .project()
                .media
                .iter()
                .map(|media| Path::new(&media.path))
                .collect();
            let rows: Vec<_> = state
                .files
                .iter()
                .filter(|path| !imported.contains(path.as_path()))
                .filter_map(|path| {
                    Some(IncomingFileData {
                        path: path.to_str()?.into(),
                        name: path.file_name()?.to_str()?.into(),
                        selected: state.selected.contains(path),
                        pending: state.pending.contains(path),
                    })
                })
                .collect();
            ui.set_folder(
                self.prefs
                    .incoming_media_folder
                    .as_deref()
                    .map(|path| path.to_string_lossy().as_ref().into())
                    .unwrap_or_default(),
            );
            ui.set_loading(state.scan_busy && !state.scan_done);
            ui.set_scan_error(state.scan_error);
            ui.set_selected_count(
                rows.iter()
                    .filter(|row| row.selected && !row.pending)
                    .count() as i32,
            );
            sync(&state.inbox_rows, rows);
            ui.set_incoming_files(state.inbox_rows.clone().into());
        }
        let media = state
            .uses_id
            .as_deref()
            .and_then(|id| self.project().media_by_id(id));
        ui.set_uses_open(media.is_some());
        if let Some(media) = media {
            let revision = self.edit_revision();
            if state.uses_revision.get() != Some(revision) {
                sync(&state.uses_rows, media_uses(self.project(), &media.id));
                state.uses_revision.set(Some(revision));
            }
            ui.set_uses_name(media.name.as_str().into());
            ui.set_uses(state.uses_rows.clone().into());
        }
    }
}

fn scan_folder(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let supported = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                MEDIA_EXTENSIONS
                    .iter()
                    .any(|supported| supported.eq_ignore_ascii_case(extension))
            });
        if supported && path.to_str().is_some() && path.is_file() {
            files.push(path);
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn shows_image(background: &Background, id: &str) -> bool {
    matches!(background, Background::Image { media_id } if media_id == id)
}

fn clip_uses_media(clip: &Clip, id: &str) -> bool {
    clip.media_id == id
        || clip
            .background
            .as_ref()
            .is_some_and(|background| shows_image(background, id))
        || clip
            .enhanced
            .as_ref()
            .is_some_and(|copy| copy.original == id || copy.copy == id)
}

fn media_uses(project: &Project, id: &str) -> Vec<MediaUseData> {
    let mut uses = Vec::new();
    for timeline in &project.timelines {
        let rate = timeline.video.rate() as f32;
        let row = |clip_id: &str, name: &str, start: f64, end: f64| MediaUseData {
            timeline_id: timeline.id.as_str().into(),
            timeline_name: crate::i18n::timeline_label(&timeline.name).into(),
            clip_id: clip_id.into(),
            label: name.into(),
            timecode: format!(
                "{} – {}",
                frames_timecode(start as f32, rate),
                frames_timecode(end as f32, rate)
            )
            .into(),
        };
        if shows_image(&timeline.background, id) {
            uses.push(row("", "", 0.0, timeline.duration()));
        }
        let mut clips: Vec<_> = timeline
            .clips
            .iter()
            .filter(|clip| clip_uses_media(clip, id))
            .collect();
        clips.sort_by(|left, right| {
            left.start
                .total_cmp(&right.start)
                .then(left.id.cmp(&right.id))
        });
        uses.extend(
            clips
                .into_iter()
                .map(|clip| row(&clip.id, &clip.name, clip.start, clip.start + clip.duration)),
        );
    }
    uses
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use concat_project::model::{Enhanced, Timeline, VideoSettings};

    use super::*;

    #[test]
    fn media_usage_spans_are_sorted_across_timelines_and_keep_the_local_frame_rate() {
        let mut project = Project::new();
        let image = Background::Image {
            media_id: "m1".into(),
        };
        let describe = |id: &str, start| Clip {
            id: id.into(),
            media_id: "m1".into(),
            start,
            ..Clip::default()
        };
        let main = project.active_mut();
        main.clips.extend([
            Arc::new(describe("later", 6.0)),
            Arc::new(Clip {
                id: "unrelated".into(),
                ..Clip::default()
            }),
            Arc::new(describe("earlier", 1.0)),
        ]);
        let other = Timeline {
            id: "other".into(),
            background: image.clone(),
            video: VideoSettings {
                rate_num: 24,
                ..VideoSettings::default()
            },
            clips: vec![
                Arc::new(Clip {
                    id: "enhanced".into(),
                    start: 2.5,
                    enhanced: Some(Enhanced {
                        original: "m1".into(),
                        copy: "m2".into(),
                        on: true,
                        offset: 0.0,
                    }),
                    ..Clip::default()
                }),
                Arc::new(Clip {
                    id: "backdrop".into(),
                    start: 4.0,
                    background: Some(image),
                    ..Clip::default()
                }),
            ],
            ..Timeline::default()
        };
        project.timelines.push(Arc::new(other));
        let rows = media_uses(&project, "m1");
        assert_eq!(
            rows.iter()
                .map(|row| row.clip_id.as_str())
                .collect::<Vec<_>>(),
            ["earlier", "later", "", "enhanced", "backdrop"]
        );
        assert_eq!(rows[2].timeline_id.as_str(), "other");
        assert_eq!(rows[2].timecode.as_str(), "00:00:00:00 – 00:00:05:00");
        assert_eq!(rows[3].timecode.as_str(), "00:00:02:12 – 00:00:03:12");
        assert!(media_uses(&project, "missing").is_empty());
    }
}
