// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use concat_export::stems::{self, StemError, StemRole};
use concat_project::model::ClipKind;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, StemExport, StemTrackData};

#[derive(Default)]
struct Draft {
    epoch: u64,
    timeline: String,
    roles: BTreeMap<String, StemRole>,
    choosing: bool,
}

thread_local! {
    static DRAFT: RefCell<Draft> = RefCell::new(Draft::default());
}

pub(crate) fn bind(app: &App) {
    let ui = app.global::<StemExport>();
    ui.on_open_library(|| {
        Shell::with(|shell, app| {
            let studio = shell.studio.borrow();
            if let Some(session) = studio.session.as_ref() {
                let ui = app.global::<StemExport>();
                if ui.get_busy() || ui.get_open() {
                    return;
                }
                DRAFT.with_borrow_mut(|draft| {
                    *draft = Draft {
                        epoch: host::project_epoch(),
                        timeline: session.project().active_timeline_id.clone(),
                        ..Draft::default()
                    };
                });
                ui.set_error(SharedString::default());
                ui.set_open(true);
                publish(&studio, &app);
            }
        })
    });
    ui.on_close(|| {
        Shell::with(|_, app| {
            let ui = app.global::<StemExport>();
            if !ui.get_busy() {
                ui.set_open(false);
            }
        })
    });
    ui.on_assign(|id, value| {
        Shell::with(|shell, app| {
            let role = match value {
                1 => Some(StemRole::Voice),
                2 => Some(StemRole::Music),
                3 => Some(StemRole::Effects),
                _ => None,
            };
            DRAFT.with_borrow_mut(|draft| {
                if let Some(role) = role {
                    draft.roles.insert(id.to_string(), role);
                } else {
                    draft.roles.remove(id.as_str());
                }
            });
            let ui = app.global::<StemExport>();
            ui.set_error(SharedString::default());
            publish(&shell.studio.borrow(), &app);
        })
    });
    ui.on_choose_destination(choose);
    ui.on_cancel(|| {
        Shell::with(|shell, app| {
            let ui = app.global::<StemExport>();
            if ui.get_busy() {
                shell.studio.borrow().host.exporter.cancel();
                ui.set_cancelled(true);
                ui.set_progress(t("stems.cancelling").into());
            }
        })
    });
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let ui = app.global::<StemExport>();
    ui.set_available(studio.live_enabled && studio.session.is_some());
    if !ui.get_open() {
        return;
    }
    let snapshot = DRAFT.with_borrow(|draft| (draft.epoch, draft.timeline.clone()));
    let Some(session) = studio.session.as_ref().filter(|session| {
        snapshot.0 == host::project_epoch() && snapshot.1 == session.project().active_timeline_id
    }) else {
        ui.set_open(false);
        ui.set_busy(false);
        return;
    };
    let project = session.project();
    let timeline = project.active();
    let valid = |clip: &&std::sync::Arc<concat_project::model::Clip>| {
        matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
            && clip.muted != Some(true)
            && project
                .media_by_id(&clip.media_id)
                .is_some_and(|media| media.has_audio)
    };
    let mut can_export = false;
    let mut rows = Vec::new();
    let roles = DRAFT.with_borrow(|draft| draft.roles.clone());
    for (index, track) in timeline.tracks.iter().rev().enumerate() {
        if track.muted {
            continue;
        }
        let Some(first) = timeline
            .clips
            .iter()
            .filter(valid)
            .find(|clip| clip.track_id == track.id)
        else {
            continue;
        };
        let role = roles.get(&track.id).map_or(0, |role| match role {
            StemRole::Voice => 1,
            StemRole::Music => 2,
            StemRole::Effects => 3,
        });
        can_export |= role != 0;
        rows.push(StemTrackData {
            id: track.id.as_str().into(),
            label: tf("stems.lane", &[&(index + 1), &first.name]).into(),
            role,
        });
    }
    if !ui.get_tracks().iter().eq(rows.iter().cloned()) {
        ui.set_tracks(ModelRc::new(VecModel::from(rows)));
    }
    ui.set_can_export(can_export);
}

fn choose() {
    Shell::with(|shell, app| {
        let studio = shell.studio.borrow();
        let ui = app.global::<StemExport>();
        let Some(session) = studio.session.as_ref() else {
            return;
        };
        if ui.get_busy() || !ui.get_can_export() {
            return;
        }
        let (epoch, timeline_id) = DRAFT.with_borrow_mut(|draft| {
            if draft.choosing {
                return (None, String::new());
            }
            draft.choosing = true;
            (Some(draft.epoch), draft.timeline.clone())
        });
        let Some(epoch) = epoch else { return };
        let folder = session.path().to_owned();
        let title = t("stems.exportFolder");
        host::after_dialog(
            move || Some(crate::platform::pick_folder(&title, &folder)),
            move |studio, app, _, picked| {
                DRAFT.with_borrow_mut(|draft| draft.choosing = false);
                if let Some(folder) = picked {
                    start(studio, app, epoch, &timeline_id, folder);
                }
            },
        );
    });
}

fn start(studio: &mut Studio, app: &App, epoch: u64, timeline_id: &str, directory: PathBuf) {
    let ui = app.global::<StemExport>();
    let Some(session) = studio.session.as_ref().filter(|session| {
        epoch == host::project_epoch() && timeline_id == session.project().active_timeline_id
    }) else {
        return;
    };
    let job = match studio.host.exporter.begin() {
        Ok(job) => job,
        Err(_) => {
            ui.set_error(t("stems.busy").into());
            return;
        }
    };
    let project = session.project().clone();
    let project_folder = PathBuf::from(session.path());
    let roles = DRAFT.with_borrow(|draft| draft.roles.clone());
    let name = concat_host::projects::folder_name(&studio.project_name) + "-audio";
    ui.set_error(SharedString::default());
    ui.set_progress(t("stems.preparing").into());
    ui.set_cancelled(false);
    ui.set_busy(true);
    host::spawn_long_in_project(
        move || {
            stems::export(
                &project,
                &project_folder,
                &roles,
                &directory,
                &name,
                job.cancel_flag(),
                |index, total, role| {
                    host::on_ui_in_project(epoch, move |_, app, _| {
                        let ui = app.global::<StemExport>();
                        if !ui.get_cancelled() {
                            ui.set_progress(
                                tf("stems.progress", &[&role_name(role), &index, &total]).into(),
                            );
                        }
                    })
                },
            )
        },
        |studio, app, _, result| {
            let ui = app.global::<StemExport>();
            ui.set_busy(false);
            ui.set_cancelled(false);
            ui.set_progress(SharedString::default());
            match result {
                Ok(folder) => {
                    ui.set_open(false);
                    studio.notify(&tf("stems.exported", &[&folder.display()]), false);
                }
                Err(error) => ui.set_error(error_text(error).into()),
            }
        },
    );
}

fn role_name(role: StemRole) -> String {
    match role {
        StemRole::Voice => t("stems.voice"),
        StemRole::Music => t("stems.music"),
        StemRole::Effects => t("stems.effects"),
    }
}

fn error_text(error: StemError) -> String {
    match error {
        StemError::Empty => t("stems.empty"),
        StemError::Timing => t("stems.length"),
        StemError::Destination => t("stems.destination"),
        StemError::Cancelled => t("stems.cancelled"),
        StemError::Files(error) => tf("stems.writeError", &[&error]),
        StemError::Mix { role, error } => tf("stems.mixError", &[&role_name(role), &error]),
    }
}
