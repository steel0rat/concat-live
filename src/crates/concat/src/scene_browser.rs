// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_host::scenes::{self, ScanError};
use concat_project::Command;
use concat_project::model::{ColorRange, MediaKind};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::host::{self, Shell};
use crate::i18n::t;
use crate::studio::Studio;
use crate::ui::{App, SceneBrowser, SceneRowData};

struct Browser {
    epoch: u64,
    media_id: String,
    media_path: String,
    file: PathBuf,
    duration: Option<f64>,
    color_range: Option<ColorRange>,
    ranges: Vec<[f64; 2]>,
    rows: Rc<VecModel<SceneRowData>>,
    cancelled: Arc<AtomicBool>,
}

impl Browser {
    fn is_current(&self, studio: &Studio) -> bool {
        self.epoch == host::project_epoch()
            && studio
                .project()
                .media_by_id(&self.media_id)
                .is_some_and(|item| {
                    item.kind == MediaKind::Video
                        && !item.placeholder
                        && item.path == self.media_path
                        && item.color_range == self.color_range
                        && item.duration == self.duration
                })
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

thread_local! {
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let global = app.global::<SceneBrowser>();
    global.on_close(|| Shell::with(|_, app| close(&app)));
    global.on_toggle(|row| Shell::with(|_, app| toggle(&app, row)));
    global.on_preview(|row| {
        Shell::with(|shell, app| preview(&mut shell.studio.borrow_mut(), &app, row))
    });
    global.on_insert(|| {
        Shell::with(|shell, app| {
            let mut studio = shell.studio.borrow_mut();
            if insert(&mut studio, &app) {
                studio.publish(&app, &shell.models);
            }
        })
    });
}

pub(crate) fn open(studio: &mut Studio, media_id: &str) {
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let Some(item) = studio.project().media_by_id(media_id) else {
        return;
    };
    if item.kind != MediaKind::Video || item.placeholder {
        return;
    }
    let name = item.name.clone();
    let file = Path::new(session.path()).join(&item.path);
    let duration = item.duration;
    let color_range = item.color_range;
    let cancelled = Arc::new(AtomicBool::new(false));
    let token = Arc::clone(&cancelled);
    let rows = Rc::new(VecModel::default());
    let browser = Browser {
        epoch: host::project_epoch(),
        media_id: item.id.clone(),
        media_path: item.path.clone(),
        file: file.clone(),
        duration,
        color_range,
        ranges: Vec::new(),
        rows: Rc::clone(&rows),
        cancelled,
    };
    Shell::with(|_, app| {
        BROWSER.with(|slot| *slot.borrow_mut() = Some(browser));
        let global = app.global::<SceneBrowser>();
        global.set_name(name.into());
        global.set_rows(ModelRc::from(rows));
        global.set_selected_count(0);
        global.set_error("".into());
        global.set_busy(true);
        global.set_open(true);
    });

    let scan_token = Arc::clone(&token);
    host::spawn_long_in_project(
        move || {
            scenes::analyse(
                &file,
                duration,
                color_range.map(concat_export::engine_range),
                &scan_token,
            )
        },
        move |studio, app, _, result| {
            BROWSER.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(browser) = slot.as_mut().filter(|browser| {
                    Arc::ptr_eq(&browser.cancelled, &token) && browser.is_current(studio)
                }) else {
                    return;
                };
                let global = app.global::<SceneBrowser>();
                global.set_busy(false);
                match result {
                    Ok(scenes) => {
                        let mut rows = Vec::with_capacity(scenes.len());
                        for scene in scenes {
                            browser.ranges.push([scene.start, scene.end]);
                            rows.push(SceneRowData {
                                time_label: format!(
                                    "{} – {}",
                                    clock(scene.start),
                                    clock(scene.end)
                                )
                                .into(),
                                thumbnail: host::image_of(&scene.thumbnail),
                                selected: false,
                            });
                        }
                        browser.rows.set_vec(rows);
                    }
                    Err(ScanError::Cancelled) => {}
                    Err(ScanError::Missing) => global.set_error(t("sceneBrowser.missing").into()),
                    Err(ScanError::TooShort) => global.set_error(t("sceneBrowser.tooShort").into()),
                    Err(ScanError::NoFrames) => {
                        global.set_error(t("sceneBrowser.decodeFailed").into())
                    }
                    Err(ScanError::Decode(error)) => {
                        log::warn!("Scene analysis failed: {error}");
                        global.set_error(t("sceneBrowser.decodeFailed").into());
                    }
                }
            });
        },
    );
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let stale = BROWSER.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|browser| !browser.is_current(studio))
    });
    if stale {
        close(app);
    }
}

fn close(app: &App) {
    BROWSER.with(|slot| *slot.borrow_mut() = None);
    let global = app.global::<SceneBrowser>();
    global.set_open(false);
    global.set_busy(false);
    global.set_selected_count(0);
    global.set_error("".into());
    global.set_rows(ModelRc::default());
}

fn toggle(app: &App, row: i32) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    BROWSER.with(|slot| {
        let slot = slot.borrow();
        let Some(browser) = slot.as_ref() else { return };
        let Some(mut item) = browser.rows.row_data(index) else {
            return;
        };
        item.selected = !item.selected;
        let delta = if item.selected { 1 } else { -1 };
        browser.rows.set_row_data(index, item);
        let global = app.global::<SceneBrowser>();
        global.set_selected_count(global.get_selected_count() + delta);
        global.set_error("".into());
    });
}

fn preview(studio: &mut Studio, app: &App, row: i32) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    let cue = BROWSER.with(|slot| {
        let slot = slot.borrow();
        let browser = slot.as_ref().filter(|browser| browser.is_current(studio))?;
        if !browser.file.is_file() {
            app.global::<SceneBrowser>()
                .set_error(t("sceneBrowser.missing").into());
            return None;
        }
        let [start, _] = *browser.ranges.get(index)?;
        let [_, end] = *browser.ranges.last()?;
        Some((browser.media_id.clone(), start, end))
    });
    let Some((media_id, start, duration)) = cue else {
        return;
    };
    let row = studio.media.row_of(&media_id).unwrap_or(-1);
    studio.preview_media_with_duration(&media_id, row, Some(duration));
    if studio.source.as_ref().is_some_and(|source| source.playing) {
        studio.source_toggle();
    }
    studio.source_seek(start);
    studio.publish_source(app);
}

fn insert(studio: &mut Studio, app: &App) -> bool {
    let global = app.global::<SceneBrowser>();
    let command = BROWSER.with(|slot| {
        let slot = slot.borrow();
        let browser = slot.as_ref().filter(|browser| browser.is_current(studio))?;
        if !browser.file.is_file() {
            global.set_error(t("sceneBrowser.missing").into());
            return None;
        }
        let ranges: Vec<_> = browser
            .ranges
            .iter()
            .copied()
            .zip(browser.rows.iter())
            .filter_map(|(range, row)| row.selected.then_some(range))
            .collect();
        (!ranges.is_empty()).then(|| Command::AddSourceRanges {
            media_id: browser.media_id.clone(),
            ranges,
            start: f64::from(studio.playhead),
        })
    });
    let Some(command) = command else { return false };
    if studio.try_apply(command).is_err() {
        global.set_error(t("sceneBrowser.insertFailed").into());
        return false;
    }
    close(app);
    studio.close_source();
    true
}

fn clock(seconds: f64) -> String {
    let millis = (seconds * 1000.0).round() as u64;
    format!(
        "{}.{:03}",
        crate::format::running_time((millis / 1000) as f64),
        millis % 1000
    )
}
