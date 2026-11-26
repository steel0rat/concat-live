// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

mod model;

use std::cell::RefCell;
use std::path::Path;

use concat_project::model::MediaItem;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::host::{self, Shell};
use crate::i18n::t;
use crate::studio::Studio;
use crate::ui::{App, PaneKind, TakeCompare};
use model::{TakeError, TakeSlot};

const SOURCE_CARD: &str = "take-comparison";

struct Comparison {
    epoch: u64,
    slot: TakeSlot,
    offered: Vec<String>,
    alternative: Option<MediaItem>,
    source_start: f64,
    showing_alternative: bool,
}

impl Comparison {
    fn is_current(&self, studio: &Studio) -> bool {
        let project = studio.project();
        self.epoch == host::project_epoch()
            && self.slot.matches(project)
            && !studio.locked(&self.slot.clip.track_id)
            && self
                .alternative
                .as_ref()
                .is_none_or(|media| project.media_by_id(&media.id) == Some(media))
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Comparison>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let ui = app.global::<TakeCompare>();
    ui.on_close(|| Shell::with(|shell, app| close(&mut shell.studio.borrow_mut(), &app)));
    ui.on_choose(|index| {
        Shell::with(|shell, app| choose(&mut shell.studio.borrow_mut(), &app, index))
    });
    ui.on_show(|alternative| {
        Shell::with(|shell, app| audition(&mut shell.studio.borrow_mut(), &app, alternative))
    });
    ui.on_change_start(|at| {
        Shell::with(|shell, app| change_start(&mut shell.studio.borrow_mut(), &app, at))
    });
    ui.on_apply(|| {
        Shell::with(|shell, app| {
            let mut studio = shell.studio.borrow_mut();
            if apply(&mut studio, &app) {
                studio.publish(&app, &shell.models);
            }
        })
    });
}

pub(crate) fn available(studio: &Studio, clip_id: &str) -> bool {
    !studio.meters.phone
        && studio.clip(clip_id).is_some_and(|clip| {
            matches!(
                clip.kind,
                concat_project::model::ClipKind::Video | concat_project::model::ClipKind::Audio
            )
        })
}

pub(crate) fn open(studio: &mut Studio, clip_id: &str) {
    let slot = match TakeSlot::capture(studio.project(), clip_id) {
        Ok(slot) => slot,
        Err(error) => {
            studio.notify(&message(error), true);
            return;
        }
    };
    if studio.locked(&slot.clip.track_id) {
        studio.notify(&t("takeCompare.locked"), true);
        return;
    }
    if !exists(studio, &slot.original) {
        studio.notify(&t("takeCompare.missing"), true);
        return;
    }

    for kind in [PaneKind::Media, PaneKind::Preview] {
        if !studio.dock.contains(kind) {
            crate::add_pane(studio, kind);
        }
    }

    let offered: Vec<_> = studio
        .project()
        .media
        .iter()
        .filter(|media| slot.accepts(media))
        .collect();
    let names: Vec<SharedString> = offered
        .iter()
        .map(|media| media.name.as_str().into())
        .collect();
    let ids = offered.iter().map(|media| media.id.clone()).collect();
    let active = Comparison {
        epoch: host::project_epoch(),
        slot,
        offered: ids,
        alternative: None,
        source_start: 0.0,
        showing_alternative: false,
    };
    Shell::with(|_, app| {
        let ui = app.global::<TakeCompare>();
        ui.set_slot(active.slot.clip.name.as_str().into());
        ui.set_original(active.slot.original.name.as_str().into());
        ui.set_candidates(ModelRc::new(VecModel::from(names)));
        ui.set_choice(-1);
        ui.set_source_start(0.0);
        ui.set_max_start(0.0);
        ui.set_ready(false);
        ui.set_showing_alternative(false);
        ui.set_error("".into());
        ACTIVE.with(|state| *state.borrow_mut() = Some(active));
        ui.set_open(true);
        audition(studio, &app, false);
    });
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let obsolete = ACTIVE.with(|state| {
        state
            .borrow()
            .as_ref()
            .is_some_and(|active| !active.is_current(studio))
    });
    if obsolete {
        dismiss(app);
        if is_take(studio) {
            host::on_ui_in_project(host::project_epoch(), |studio, _, _| {
                if ACTIVE.with(|state| state.borrow().is_none()) && is_take(studio) {
                    studio.close_source();
                }
            });
        }
    }
}

fn choose(studio: &mut Studio, app: &App, index: i32) {
    let ui = app.global::<TakeCompare>();
    let selected = ACTIVE.with(|state| {
        let mut state = state.borrow_mut();
        let active = state
            .as_mut()
            .filter(|active| active.is_current(studio))
            .ok_or(TakeError::Stale)?;
        let id = usize::try_from(index)
            .ok()
            .and_then(|index| active.offered.get(index));
        let media = id
            .and_then(|id| studio.project().media_by_id(id))
            .ok_or(TakeError::Stale)?;
        active.alternative = Some(media.clone());
        active.source_start = 0.0;
        active.slot.limit(media)
    });
    ui.set_choice(index);
    ui.set_source_start(0.0);
    ui.set_ready(selected.is_ok());
    ui.set_max_start(selected.as_ref().map_or(0.0, |seconds| *seconds) as f32);
    match selected {
        Ok(_) => audition(studio, app, true),
        Err(error) => {
            audition(studio, app, false);
            ui.set_error(message(error).into());
        }
    }
}

fn change_start(studio: &mut Studio, app: &App, start: f32) {
    if !start.is_finite() {
        app.global::<TakeCompare>()
            .set_error(t("takeCompare.invalidInPoint").into());
        return;
    }
    let show = ACTIVE.with(|state| {
        let mut state = state.borrow_mut();
        let active = state.as_mut().filter(|active| active.is_current(studio))?;
        let alternative = active.alternative.as_ref()?;
        let limit = active.slot.limit(alternative).ok()?;
        active.source_start = f64::from(start).clamp(0.0, limit);
        Some(active.showing_alternative)
    });
    if show == Some(true) {
        audition(studio, app, true);
    }
}

fn audition(studio: &mut Studio, app: &App, alternative: bool) {
    let ui = app.global::<TakeCompare>();
    let plan = ACTIVE.with(|state| {
        let state = state.borrow();
        let active = state
            .as_ref()
            .filter(|active| active.is_current(studio))
            .ok_or(TakeError::Stale)?;
        let (media, replacement) = if alternative {
            let media = active.alternative.as_ref().ok_or(TakeError::Incompatible)?;
            (media, Some((media, active.source_start)))
        } else {
            (&active.slot.original, None)
        };
        let clip = &active.slot.clip;
        let at = f64::from(studio.playhead);
        let mut cursor = if (clip.start..clip.start + clip.duration).contains(&at) {
            at - clip.start
        } else {
            0.0
        };
        let mut playing = false;
        if let Some(source) = studio
            .source
            .as_ref()
            .filter(|source| source.card == SOURCE_CARD)
        {
            cursor = source.time.min((clip.duration - 1e-3).max(0.0));
            playing = source.playing;
        }
        let scratch = active.slot.preview_project(studio.project(), replacement)?;
        Ok((scratch, media.clone(), cursor, playing))
    });
    let (scratch, media, at, playing) = match plan {
        Ok(plan) => plan,
        Err(error) => {
            ui.set_error(message(error).into());
            return;
        }
    };
    if !exists(studio, &media) {
        ui.set_error(t("takeCompare.missing").into());
        return;
    }
    if !studio.preview_take(&scratch, &media.name, SOURCE_CARD, at, playing) {
        ui.set_error(t("takeCompare.previewFailed").into());
        return;
    }
    ACTIVE.with(|state| {
        if let Some(active) = state.borrow_mut().as_mut() {
            active.showing_alternative = alternative;
        }
    });
    ui.set_showing_alternative(alternative);
    ui.set_error("".into());
    studio.publish_source(app);
}

fn apply(studio: &mut Studio, app: &App) -> bool {
    let ui = app.global::<TakeCompare>();
    let plan = ACTIVE.with(|state| {
        let state = state.borrow();
        let active = state
            .as_ref()
            .filter(|active| active.is_current(studio))
            .ok_or(TakeError::Stale)?;
        let alternative = active.alternative.as_ref().ok_or(TakeError::Incompatible)?;
        Ok((
            active
                .slot
                .replacement(studio.project(), alternative, active.source_start)?,
            alternative.clone(),
        ))
    });
    let (command, media) = match plan {
        Ok(plan) => plan,
        Err(error) => {
            ui.set_error(message(error).into());
            return false;
        }
    };
    if !exists(studio, &media) {
        ui.set_error(t("takeCompare.missing").into());
        return false;
    }
    if studio.try_apply(command).is_err() {
        ui.set_error(t("takeCompare.applyFailed").into());
        return false;
    }
    close(studio, app);
    true
}

fn exists(studio: &Studio, media: &MediaItem) -> bool {
    studio
        .session
        .as_ref()
        .is_some_and(|session| Path::new(session.path()).join(&media.path).is_file())
}

fn is_take(studio: &Studio) -> bool {
    studio
        .source
        .as_ref()
        .is_some_and(|source| source.card == SOURCE_CARD)
}

fn close(studio: &mut Studio, app: &App) {
    dismiss(app);
    if is_take(studio) {
        studio.close_source();
        studio.publish_source(app);
    }
}

fn dismiss(app: &App) {
    ACTIVE.with(|state| *state.borrow_mut() = None);
    let ui = app.global::<TakeCompare>();
    ui.set_open(false);
    ui.set_candidates(ModelRc::default());
}

fn message(error: TakeError) -> String {
    match error {
        TakeError::Stale => t("takeCompare.stale"),
        TakeError::Unsupported => t("takeCompare.unsupported"),
        TakeError::SeparateAudio => t("takeCompare.separateAudio"),
        TakeError::Incompatible => t("takeCompare.incompatible"),
        TakeError::UnknownLength => t("takeCompare.unknownLength"),
        TakeError::InvalidInPoint => t("takeCompare.invalidInPoint"),
        TakeError::TooShort => t("takeCompare.tooShort"),
        TakeError::MissingStream => t("takeCompare.missingStream"),
    }
}
