// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use std::cell::RefCell;
use std::path::PathBuf;

use concat_project::Command;
use concat_project::model::Timeline;
use concat_project::sections::{
    ChapterError, Section, SectionAnchor, SectionTiming, publication_chapters,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::format::{frames_timecode, running_time};
use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, SectionPlanning, SectionRowData};

thread_local! {
    static TIMELINE: RefCell<Option<(u64, String)>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let ui = app.global::<SectionPlanning>();
    ui.on_toggle(|| {
        handle(|studio, ui| {
            if let Some(timeline) = active(studio) {
                TIMELINE.with_borrow_mut(|previous| {
                    *previous = Some((host::project_epoch(), timeline.id.clone()));
                });
            }
            ui.set_open(!ui.get_open() && studio.session.is_some());
            clear_form(ui);
        })
    });
    ui.on_close(|| {
        handle(|_, ui| {
            ui.set_open(false);
            clear_form(ui);
        })
    });
    ui.on_new_section(|| handle(|_, ui| clear_form(ui)));
    ui.on_select(|id| handle(|studio, ui| select(studio, ui, id)));
    ui.on_jump(|id| {
        handle(|studio, ui| {
            if let Some(timeline) = active(studio)
                && let Some(row) = SectionTiming::on_timeline(&timeline.sections, timeline)
                    .into_iter()
                    .find(|row| row.section.id as i32 == id)
            {
                studio.seek(row.start as f32);
                ui.set_error(SharedString::default());
            }
        })
    });
    ui.on_remove(|id| handle(|studio, ui| remove(studio, ui, id)));
    ui.on_save(|move_start| handle(|studio, ui| save(studio, ui, move_start)));
    ui.on_export_chapters(|| handle(|studio, ui| export_chapters(studio, ui)));
}

fn handle(work: impl FnOnce(&mut Studio, &SectionPlanning<'_>)) {
    Shell::with(|shell, app| {
        {
            let mut studio = shell.studio.borrow_mut();
            work(&mut studio, &app.global::<SectionPlanning>());
        }
        shell.studio.borrow().publish(&app, &shell.models);
    });
}

fn clear_form(ui: &SectionPlanning<'_>) {
    ui.set_editing_id(0);
    ui.set_name(SharedString::default());
    ui.set_planned(SharedString::default());
    ui.set_follow_clip(false);
    ui.set_error(SharedString::default());
}

fn active(studio: &Studio) -> Option<&Timeline> {
    Some(studio.session.as_ref()?.project().active())
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let ui = app.global::<SectionPlanning>();
    let timeline = active(studio);
    ui.set_available(studio.live_enabled && timeline.is_some());
    if !ui.get_open() {
        return;
    }
    let Some(timeline) = timeline else {
        clear_form(&ui);
        ui.set_open(false);
        return;
    };
    TIMELINE.with_borrow_mut(|previous| {
        let epoch = host::project_epoch();
        if let Some((previous_epoch, previous_timeline)) = previous {
            if *previous_epoch != epoch {
                ui.set_open(false);
            }
            if *previous_epoch != epoch || *previous_timeline != timeline.id {
                clear_form(&ui);
            }
        }
        *previous = Some((epoch, timeline.id.clone()));
    });
    ui.set_timeline(crate::i18n::timeline_label(&timeline.name).into());
    publish_cursor(studio, app);
    ui.set_can_follow(matches!(studio.selection.as_slice(), [id] if timeline.clip(id).is_some()));
    if ui.get_editing_id() != 0
        && !timeline
            .sections
            .iter()
            .any(|section| section.id as i32 == ui.get_editing_id())
    {
        clear_form(&ui);
    }
    let rows = SectionTiming::on_timeline(&timeline.sections, timeline);
    ui.set_rows(ModelRc::new(VecModel::from(
        rows.iter()
            .map(|row| {
                let duration = row.duration();
                SectionRowData {
                    id: row.section.id as i32,
                    name: row.section.name.as_str().into(),
                    start: frames_timecode(row.start as f32, studio.frame_rate()).into(),
                    timing: {
                        let actual = decimal(duration, false);
                        match row.section.planned {
                            Some(seconds) => {
                                let planned = decimal(seconds, false);
                                let difference = decimal(duration - seconds, true);
                                tf("sections.compare", &[&actual, &planned, &difference]).into()
                            }
                            None => tf("sections.actual", &[&actual]).into(),
                        }
                    },
                    attention: !row.attached || duration <= 0.0,
                }
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_can_export(!rows.is_empty());
}

pub(crate) fn publish_cursor(studio: &Studio, app: &App) {
    let ui = app.global::<SectionPlanning>();
    if ui.get_open() {
        ui.set_cursor(frames_timecode(studio.playhead, studio.frame_rate()).into());
    }
}

fn decimal(seconds: f64, with_sign: bool) -> String {
    let rounded = (seconds * 100.0).round() / 100.0;
    let words = if with_sign && rounded != 0.0 {
        format!("{rounded:+.2}")
    } else {
        format!("{:.2}", if rounded == 0.0 { 0.0 } else { rounded })
    };
    if crate::i18n::current() == "ru" {
        words.replace('.', ",")
    } else {
        words
    }
}

fn select(studio: &Studio, ui: &SectionPlanning<'_>, id: i32) {
    if let Some(section) = active(studio).and_then(|timeline| {
        timeline
            .sections
            .iter()
            .find(|section| section.id as i32 == id)
    }) {
        clear_form(ui);
        ui.set_editing_id(id);
        ui.set_name(section.name.as_str().into());
        ui.set_planned(
            section
                .planned
                .map(|seconds| seconds.to_string())
                .unwrap_or_default()
                .into(),
        );
    }
}

fn remove(studio: &mut Studio, ui: &SectionPlanning<'_>, id: i32) {
    let Some(timeline) = active(studio) else {
        return;
    };
    let mut sections = timeline.sections.clone();
    sections.retain(|section| section.id as i32 != id);
    let timeline_id = timeline.id.clone();
    let result = apply(studio, timeline_id, sections);
    if result.is_ok() && ui.get_editing_id() == id {
        clear_form(ui);
    }
    ui.set_error(result.err().unwrap_or_default().into());
}

fn save(studio: &mut Studio, ui: &SectionPlanning<'_>, move_start: bool) {
    let result = save_form(studio, ui, move_start);
    match result {
        Ok(()) => clear_form(ui),
        Err(error) => ui.set_error(error.into()),
    }
}

fn save_form(
    studio: &mut Studio,
    ui: &SectionPlanning<'_>,
    move_start: bool,
) -> Result<(), String> {
    let timeline = active(studio).ok_or_else(|| t("sections.noTimeline"))?;
    let mut sections = timeline.sections.clone();
    let editing = u32::try_from(ui.get_editing_id()).unwrap_or(0);
    let existing = sections.iter_mut().find(|section| section.id == editing);
    let name = ui.get_name().trim().to_owned();
    if name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control) {
        return Err(t("sections.invalidName"));
    }
    let planned = ui.get_planned();
    let planned = if planned.trim().is_empty() {
        None
    } else {
        Some(
            planned
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .ok()
                .filter(|seconds| (0.0..=720_000.0).contains(seconds))
                .ok_or_else(|| t("sections.invalidPlan"))?,
        )
    };
    let anchor = if existing.is_none() || move_start {
        let at = f64::from(studio.playhead);
        if at >= timeline.duration() {
            return Err(t("sections.cursorOutside"));
        }
        if ui.get_follow_clip() {
            let [selected] = studio.selection.as_slice() else {
                return Err(t("sections.selectClip"));
            };
            let clip = timeline
                .clip(selected)
                .filter(|clip| clip.start <= at && at < clip.start + clip.duration)
                .ok_or_else(|| t("sections.selectClip"))?;
            Some(SectionAnchor::Clip {
                clip_id: clip.id.clone(),
                offset: at - clip.start,
                fallback: at,
            })
        } else {
            Some(SectionAnchor::Time { at })
        }
    } else {
        None
    };
    if let Some(section) = existing {
        section.name = name;
        section.planned = planned;
        if let Some(anchor) = anchor {
            section.anchor = anchor;
        }
    } else {
        let id = (1..=1024)
            .find(|id| !sections.iter().any(|section| section.id == *id))
            .ok_or_else(|| t("sections.limit"))?;
        sections.push(Section {
            id,
            name,
            anchor: anchor.expect("a new section has an anchor"),
            planned,
        });
    }
    let timeline_id = timeline.id.clone();
    apply(studio, timeline_id, sections)
}

fn apply(studio: &mut Studio, timeline_id: String, sections: Vec<Section>) -> Result<(), String> {
    studio
        .try_apply(Command::SetTimelineSections {
            timeline_id,
            sections,
        })
        .map(|_| ())
        .map_err(|_| t("sections.saveError"))
}

fn chapters(timeline: &Timeline) -> Result<String, ChapterError> {
    let rows = SectionTiming::on_timeline(&timeline.sections, timeline);
    let mut result = String::new();
    for (second, name) in publication_chapters(&rows)? {
        result.push_str(&running_time(second as f64));
        result.push(' ');
        result.push_str(name);
        result.push('\n');
    }
    Ok(result)
}

fn chapter_error(error: ChapterError) -> String {
    match error {
        ChapterError::Empty => t("sections.chaptersEmpty"),
        ChapterError::MissingBeginning => t("sections.chaptersBeginning"),
        ChapterError::MissingAnchor => t("sections.chaptersAnchor"),
        ChapterError::AmbiguousTime => t("sections.chaptersTime"),
    }
}

fn export_chapters(studio: &Studio, ui: &SectionPlanning<'_>) {
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let timeline = session.project().active();
    if let Err(error) = chapters(timeline) {
        ui.set_error(chapter_error(error).into());
        return;
    }
    let timeline_id = timeline.id.clone();
    let folder = PathBuf::from(session.path());
    let epoch = host::project_epoch();
    let title = t("sections.export");
    let family = t("sections.textFiles");
    let filename = format!(
        "{}-chapters.txt",
        concat_host::projects::folder_name(&timeline.name)
    );
    host::after_dialog(
        move || crate::platform::save_file(&title, &folder, &filename, (family.as_str(), &["txt"])),
        move |studio, app, _, path| {
            let ui = app.global::<SectionPlanning>();
            let result = (|| {
                let timeline = active(studio)
                    .filter(|timeline| host::project_epoch() == epoch && timeline.id == timeline_id)
                    .ok_or_else(|| t("sections.noTimeline"))?;
                let text = chapters(timeline).map_err(chapter_error)?;
                let path = path.with_extension("txt");
                std::fs::write(&path, text)
                    .map_err(|error| tf("sections.exportError", &[&error]))?;
                Ok::<_, String>(path)
            })();
            match result {
                Ok(path) => {
                    ui.set_error("".into());
                    studio.notify(&tf("sections.exported", &[&path.display()]), false);
                }
                Err(error) => ui.set_error(error.into()),
            }
        },
    );
}
