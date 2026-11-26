// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

mod audio;
mod model;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use concat_host::playback::Playback;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::{Gesture, Studio};
use crate::ui::{App, ClickReview, ClickRowData};
use audio::Detected;
use model::{FadeError, Review};

struct Audition {
    row: i32,
    after: bool,
    cancelled: Arc<AtomicBool>,
    rendering: bool,
}

struct State {
    epoch: u64,
    revision: u64,
    timeline: String,
    review: Review,
    results: Vec<Detected>,
    rows: Rc<VecModel<ClickRowData>>,
    cancelled: Arc<AtomicBool>,
    playback: Arc<Playback>,
    audition: Option<Audition>,
    timer: slint::Timer,
}

impl State {
    fn is_current(&self, studio: &Studio) -> bool {
        self.epoch == host::project_epoch()
            && self.revision == studio.edit_revision()
            && studio
                .session
                .as_ref()
                .is_some_and(|session| session.project().active_timeline_id == self.timeline)
    }

    fn stop(&mut self) {
        if let Some(audition) = self.audition.take() {
            audition.cancelled.store(true, Ordering::Relaxed);
            self.playback.stop_audition();
        }
        self.timer.stop();
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.stop();
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let review = app.global::<ClickReview>();
    review.on_close(|| Shell::with(|_, app| close(&app)));
    review.on_toggle(|row| Shell::with(|_, app| toggle(&app, row)));
    review.on_audition(|row, after| {
        Shell::with(|shell, app| audition(&mut shell.studio.borrow_mut(), &app, row, after));
    });
    review.on_apply(|| {
        Shell::with(|shell, app| {
            let mut studio = shell.studio.borrow_mut();
            if apply(&mut studio, &app) {
                studio.publish(&app, &shell.models);
            }
        })
    });
}

pub(crate) fn open(studio: &mut Studio) {
    if !matches!(studio.gesture, Gesture::None) || studio.confirm.is_some() {
        studio.notify(&t("clickReview.finishEdit"), true);
        return;
    }
    studio.flush_commit();
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let draft = Review::capture(session.project(), |id| studio.locked(id));
    let joins = draft.joins.clone();
    let snapshot = session.snapshot();
    let rows = Rc::new(VecModel::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = Arc::clone(&cancelled);
    let token = Arc::clone(&cancelled);

    let state = State {
        epoch: host::project_epoch(),
        revision: studio.edit_revision(),
        timeline: session.project().active_timeline_id.clone(),
        review: draft,
        results: Vec::new(),
        rows: Rc::clone(&rows),
        cancelled,
        playback: Arc::clone(&studio.host.playback),
        audition: None,
        timer: slint::Timer::default(),
    };
    Shell::with(|_, app| {
        let ui = app.global::<ClickReview>();
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(state));
        ui.set_rows(ModelRc::from(rows));
        ui.set_busy(!joins.is_empty());
        ui.set_error("".into());
        ui.set_active_row(-1);
        ui.set_chosen(0);
        ui.set_open(true);
    });
    if joins.is_empty() {
        return;
    }

    host::spawn_long_in_project(
        move || {
            audio::detect(
                &snapshot.flattened_clips(),
                snapshot.project().active().duration(),
                &joins,
                &worker,
            )
        },
        move |studio, app, _, result| {
            let ui = app.global::<ClickReview>();
            ACTIVE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(state) = slot.as_mut().filter(|state| {
                    state.is_current(studio) && Arc::ptr_eq(&token, &state.cancelled)
                }) else {
                    return;
                };
                ui.set_busy(false);
                match result {
                    Ok(results) => {
                        let rows: Vec<_> = results
                            .iter()
                            .map(|found| {
                                let join = &state.review.joins[found.join];
                                let left = studio
                                    .clip(&join.left_id)
                                    .map_or("", |clip| clip.name.as_str());
                                let right = studio
                                    .clip(&join.right_id)
                                    .map_or("", |clip| clip.name.as_str());
                                log::debug!(
                                    "Audio join {:.3}s on {}: step {} samples",
                                    join.at,
                                    join.track_id,
                                    found.sample_step
                                );
                                ClickRowData {
                                    stamp: crate::format::frames_timecode(
                                        join.at as f32,
                                        studio.frame_rate(),
                                    )
                                    .into(),
                                    track: tf("clickReview.track", &[&(join.track + 1)]).into(),
                                    clips: format!("{left} → {right}").into(),
                                    selected: false,
                                }
                            })
                            .collect();
                        state.results = results;
                        state.rows.set_vec(rows);
                    }
                    Err(audio::ScanError::Cancelled) => {}
                    Err(audio::ScanError::Mix(error)) => {
                        log::warn!("Click scan failed: {error}");
                        ui.set_error(t("clickReview.readFailed").into());
                    }
                }
            });
        },
    );
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    if ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|state| !state.is_current(studio))
    }) {
        close(app);
    }
}

fn close(app: &App) {
    ACTIVE.with(|slot| *slot.borrow_mut() = None);
    let ui = app.global::<ClickReview>();
    ui.set_open(false);
    ui.set_rows(ModelRc::default());
}

fn toggle(app: &App, row: i32) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    ACTIVE.with(|slot| {
        let slot = slot.borrow();
        let Some(state) = slot.as_ref() else { return };
        let Some(mut data) = state.rows.row_data(index) else {
            return;
        };
        data.selected = !data.selected;
        let delta = if data.selected { 1 } else { -1 };
        state.rows.set_row_data(index, data);
        let ui = app.global::<ClickReview>();
        ui.set_chosen(ui.get_chosen() + delta);
    });
}

fn audition(studio: &mut Studio, app: &App, row: i32, after: bool) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let ui = app.global::<ClickReview>();
    let request = ACTIVE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.as_mut().filter(|state| state.is_current(studio))?;
        let found = state.results.get(index)?;
        let join = &state.review.joins[found.join];
        let (at, join_index) = (join.at, found.join);
        let stop_only = state
            .audition
            .as_ref()
            .is_some_and(|item| item.row == row && item.after == after);
        state.stop();
        ui.set_active_row(-1);
        if stop_only {
            return None;
        }
        let preview = if after {
            match state
                .review
                .fades(session.project(), &[join_index], |id| studio.locked(id))
            {
                Ok(command) => Some(command),
                Err(error) => {
                    ui.set_error(message(error).into());
                    return None;
                }
            }
        } else {
            None
        };
        let token = Arc::new(AtomicBool::new(false));
        state.audition = Some(Audition {
            row,
            after,
            cancelled: Arc::clone(&token),
            rendering: true,
        });
        ui.set_active_row(row);
        ui.set_after(after);
        ui.set_error("".into());
        Some((session.snapshot(), preview, at, token))
    });
    let Some((mut snapshot, preview, at, token)) = request else {
        return;
    };
    studio.close_source();
    studio.pause();
    studio.publish_source(app);
    let worker = Arc::clone(&token);
    host::spawn_long_in_project(
        move || {
            if let Some(command) = preview {
                snapshot.apply(command)?;
            }
            audio::render_audition(
                &snapshot.flattened_clips(),
                snapshot.project().active().duration(),
                at,
                &worker,
            )
            .map_err(|error| format!("{error:?}"))
        },
        move |studio, app, _, samples| {
            let ui = app.global::<ClickReview>();
            ACTIVE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(state) = slot.as_mut().filter(|state| state.is_current(studio)) else {
                    return;
                };
                let Some(item) = state
                    .audition
                    .as_mut()
                    .filter(|item| Arc::ptr_eq(&token, &item.cancelled))
                else {
                    return;
                };
                match samples {
                    Ok(samples)
                        if !samples.is_empty() && !studio.playing && studio.source.is_none() =>
                    {
                        state.playback.play_audition(samples.into());
                        item.rendering = false;
                        state.timer.start(
                            slint::TimerMode::Repeated,
                            Duration::from_millis(80),
                            poll,
                        );
                    }
                    other => {
                        if let Err(error) = other {
                            log::warn!("Click audition could not be rendered: {error}");
                        }
                        state.stop();
                        ui.set_active_row(-1);
                        ui.set_error(t("clickReview.auditionFailed").into());
                    }
                }
            });
        },
    );
}

fn poll() {
    Shell::with(|_, app| {
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(state) = slot.as_mut() else { return };
            if state.audition.as_ref().is_some_and(|item| !item.rendering)
                && !state.playback.is_auditioning()
            {
                state.stop();
                app.global::<ClickReview>().set_active_row(-1);
            }
        });
    });
}

fn apply(studio: &mut Studio, app: &App) -> bool {
    if !matches!(studio.gesture, Gesture::None) || studio.confirm.is_some() {
        return false;
    }
    studio.flush_commit();
    let draft = ACTIVE.with(|slot| {
        let slot = slot.borrow();
        let state = slot
            .as_ref()
            .filter(|state| state.is_current(studio))
            .ok_or(FadeError::ProjectChanged)?;
        let chosen: Vec<_> = state
            .results
            .iter()
            .zip(state.rows.iter())
            .filter_map(|(found, item)| item.selected.then_some(found.join))
            .collect();
        state
            .review
            .fades(studio.project(), &chosen, |id| studio.locked(id))
    });
    let ui = app.global::<ClickReview>();
    let command = match draft {
        Ok(command) => command,
        Err(error) => {
            ui.set_error(message(error).into());
            return false;
        }
    };
    if studio.try_apply(command).is_err() {
        ui.set_error(t("clickReview.applyFailed").into());
        return false;
    }
    close(app);
    true
}

fn message(error: FadeError) -> String {
    match error {
        FadeError::TrackLocked => t("clickReview.locked"),
        FadeError::ProjectChanged | FadeError::UnknownJoin => t("clickReview.changed"),
        FadeError::NothingToChange => t("clickReview.noSelection"),
    }
}
