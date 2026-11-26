// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use concat_effects::Catalogue;
use concat_host::blocks as files;
use concat_project::{
    Command,
    commands::blocks::{self, Block, Link},
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, MontageBlockCard, MontageBlocks};

struct Library {
    epoch: u64,
    pending: Option<PendingSelection>,
}

struct PendingSelection {
    revision: u64,
    timeline: String,
    selected: Vec<String>,
    requirements: Vec<blocks::RequiredClip>,
}

thread_local! {
    static LIBRARY: RefCell<Option<Library>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let global = app.global::<MontageBlocks>();
    global.on_open_library(|| Shell::with(|shell, app| open(&shell.studio.borrow(), &app)));
    global.on_closed(|| {
        Shell::with(|_, app| {
            if !app.global::<MontageBlocks>().get_busy() {
                close(&app);
            }
        })
    });
    global.on_save_selection(|include| {
        Shell::with(|shell, app| save(&mut shell.studio.borrow_mut(), &app, include));
    });
    global.on_insert_block(|id| {
        Shell::with(|shell, app| insert(&mut shell.studio.borrow_mut(), &app, id.to_string()));
    });
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let global = app.global::<MontageBlocks>();
    global.set_available(studio.session.is_some());
    global.set_can_save(studio.session.is_some() && !studio.selection.is_empty());
    let stale = LIBRARY.with(|slot| {
        slot.borrow().as_ref().is_some_and(|library| {
            studio.session.is_none() || library.epoch != host::project_epoch()
        })
    });
    if stale {
        close(app);
    }
}

fn open(studio: &Studio, app: &App) {
    let global = app.global::<MontageBlocks>();
    if studio.session.is_none() || global.get_open() {
        return;
    }
    LIBRARY.with(|slot| {
        *slot.borrow_mut() = Some(Library {
            epoch: host::project_epoch(),
            pending: None,
        })
    });
    global.set_can_save(!studio.selection.is_empty());
    global.set_name(SharedString::default());
    global.set_blocks(ModelRc::default());
    global.set_open(true);
    begin(app, t("blocks.loading"));

    let config = studio.host.dirs.config.clone();
    host::spawn_long_in_project(
        move || files::list(&config),
        |_, app, _, cards| {
            finish(app);
            show_cards(app, cards);
        },
    );
}

fn close(app: &App) {
    LIBRARY.with(|slot| *slot.borrow_mut() = None);
    let global = app.global::<MontageBlocks>();
    global.set_open(false);
    global.set_busy(false);
    global.set_error(SharedString::default());
    global.set_info(SharedString::default());
    global.set_dependencies(ModelRc::default());
}

fn save(studio: &mut Studio, app: &App, include_dependencies: bool) {
    let global = app.global::<MontageBlocks>();
    if !global.get_open() || global.get_busy() {
        return;
    }
    let name = global.get_name().trim().to_owned();
    if name.is_empty() {
        global.set_error(t("blocks.emptyName").into());
        return;
    }
    studio.flush_commit();
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let mut selected = studio.selection.clone();
    let timeline = studio.timeline().id.clone();
    let revision = studio.edit_revision();
    let pending = LIBRARY.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .and_then(|library| library.pending.take())
    });
    global.set_dependencies(ModelRc::default());
    if include_dependencies {
        let Some(checked) = pending.filter(|checked| {
            checked.revision == revision
                && checked.timeline == timeline
                && checked.selected == selected
        }) else {
            global.set_error(t("blocks.projectChanged").into());
            return;
        };
        selected.extend(
            checked
                .requirements
                .into_iter()
                .map(|requirement| requirement.id),
        );
    }

    let block = match Block::capture(studio.project(), &selected) {
        Ok(block) => block,
        Err(blocks::Error::RequiredClips(requirements)) => {
            let rows: Vec<SharedString> = requirements
                .iter()
                .map(|dependency| {
                    let names: [&dyn std::fmt::Display; 2] =
                        [&dependency.name, &dependency.linked_to];
                    match dependency.link {
                        Link::DetachedAudio => tf("blocks.linkedAudio", &names),
                        Link::Transition => tf("blocks.linkedTransition", &names),
                    }
                    .into()
                })
                .collect();
            LIBRARY.with(|slot| {
                if let Some(library) = slot.borrow_mut().as_mut() {
                    library.pending = Some(PendingSelection {
                        revision,
                        timeline,
                        selected,
                        requirements,
                    });
                }
            });
            global.set_error(SharedString::default());
            global.set_info(SharedString::default());
            global.set_dependencies(ModelRc::from(Rc::new(VecModel::from(rows))));
            return;
        }
        Err(error) => {
            global.set_error(model_error(&error).into());
            return;
        }
    };
    let config = studio.host.dirs.config.clone();
    let source = PathBuf::from(session.path());
    begin(app, t("blocks.saving"));
    host::spawn_long_in_project(
        move || {
            files::save(&config, &source, &name, block, Catalogue::builtin())
                .map(|saved| (saved, files::list(&config)))
        },
        |_, app, _, result| {
            finish(app);
            let global = app.global::<MontageBlocks>();
            match result {
                Ok((saved, cards)) => {
                    global.set_name(SharedString::default());
                    global.set_info(tf("blocks.saved", &[&saved.name]).into());
                    show_cards(app, cards);
                }
                Err(error) => global.set_error(file_error(&error).into()),
            }
        },
    );
}

fn insert(studio: &mut Studio, app: &App, id: String) {
    let global = app.global::<MontageBlocks>();
    if !global.get_open() || global.get_busy() {
        return;
    }
    studio.flush_commit();
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let destination = PathBuf::from(session.path());
    let config = studio.host.dirs.config.clone();
    let project = studio.project().clone();
    let timeline = studio.timeline().id.clone();
    let original_count = studio.timeline().clips.len();
    let revision = studio.edit_revision();
    let start = f64::from(studio.playhead);
    begin(app, t("blocks.inserting"));
    host::spawn_long_in_project(
        move || files::prepare(&config, &id, &destination, &project, Catalogue::builtin()),
        move |studio, app, models, result| {
            finish(app);
            let global = app.global::<MontageBlocks>();
            if studio.edit_revision() != revision || studio.timeline().id != timeline {
                global.set_error(t("blocks.projectChanged").into());
                return;
            }
            let prepared = match result {
                Ok(block) => block,
                Err(error) => {
                    global.set_error(file_error(&error).into());
                    return;
                }
            };
            if start < 0.0
                || start + prepared.block().project().active().duration()
                    > concat_project::model::ranges::MAX_TIME
            {
                global.set_error(t("blocks.invalidTime").into());
                return;
            }
            let command = Command::InsertBlock {
                block: Box::new(prepared.block().clone()),
                start,
            };
            match studio.try_apply(command) {
                Ok(view) => {
                    prepared.keep();
                    studio.selection = view
                        .project
                        .active()
                        .clips
                        .iter()
                        .skip(original_count)
                        .map(|clip| clip.id.clone())
                        .collect();
                    global.set_info(t("blocks.inserted").into());
                    studio.publish(app, models);
                }
                Err(_) => global.set_error(t("blocks.insertFailed").into()),
            }
        },
    );
}

fn show_cards(app: &App, cards: Vec<files::Card>) {
    let rows: Vec<_> = cards
        .into_iter()
        .map(|card| MontageBlockCard {
            id: card.id.into(),
            name: card.name.into(),
            detail: tf(
                "blocks.summary",
                &[&card.clips.to_string(), &format!("{:.2}", card.seconds)],
            )
            .into(),
        })
        .collect();
    app.global::<MontageBlocks>()
        .set_blocks(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn begin(app: &App, info: String) {
    let global = app.global::<MontageBlocks>();
    global.set_busy(true);
    global.set_error(SharedString::default());
    global.set_info(info.into());
}

fn finish(app: &App) {
    let global = app.global::<MontageBlocks>();
    global.set_busy(false);
    global.set_info(SharedString::default());
}

fn file_error(error: &files::Error) -> String {
    match error {
        files::Error::EmptyName => t("blocks.emptyName"),
        files::Error::InvalidBundle => t("blocks.invalid"),
        files::Error::File(detail) => {
            log::warn!("Montage block asset could not be accessed: {detail}");
            t("blocks.fileError")
        }
        files::Error::DamagedAsset(file) => tf("blocks.damaged", &[file]),
        files::Error::MissingEffect(effect) => tf("blocks.effectMissing", &[effect]),
        files::Error::DifferentEffect {
            id,
            expected,
            installed,
        } => {
            log::warn!(
                "Montage block requires {id} version {expected}, version {installed} is installed"
            );
            tf("blocks.effectVersion", &[id])
        }
        files::Error::Model(error) => model_error(error),
    }
}

fn model_error(error: &blocks::Error) -> String {
    match error {
        blocks::Error::EmptySelection => t("blocks.noSelection"),
        blocks::Error::MissingClip(_) => t("blocks.missingClip"),
        blocks::Error::MissingTrack(_) => t("blocks.missingTrack"),
        blocks::Error::MissingMedia(_) => t("blocks.missingMedia"),
        blocks::Error::MissingTransitionSource(_) => t("blocks.missingTransition"),
        blocks::Error::RequiredClips(_) => t("blocks.dependencies"),
        blocks::Error::InvalidTime => t("blocks.invalidTime"),
        blocks::Error::InvalidDocument => t("blocks.invalid"),
        blocks::Error::FontConflict(name) => tf("blocks.fontConflict", &[name]),
        blocks::Error::MediaConflict(_) => t("blocks.mediaConflict"),
    }
}
