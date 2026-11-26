// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

mod size;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use concat_export::{Reporter, export_folder::OutputSet, render_review_on};
use concat_host::{
    Titles,
    export::{ExportSpec, VideoCodec},
    preview::Monitor,
};
use concat_project::Project;
use slint::ComponentHandle;

use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, ReviewCopy};

const VIDEO: &str = "review.mp4";
const NOTES: &str = "notes.txt";

struct Draft {
    epoch: u64,
    timeline: String,
    choosing: bool,
    cancel: Option<Arc<AtomicBool>>,
}

impl Draft {
    fn current(&self, studio: &Studio) -> bool {
        host::project_epoch() == self.epoch
            && studio
                .session
                .as_ref()
                .is_some_and(|session| session.project().active_timeline_id == self.timeline)
    }
}

impl Drop for Draft {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

thread_local! {
    static DRAFT: RefCell<Option<Draft>> = const { RefCell::new(None) };
}

pub(crate) fn bind(app: &App) {
    let sheet = app.global::<ReviewCopy>();
    sheet.on_choose(choose_folder);
    sheet.on_cancel(|| {
        Shell::with(|_, app| {
            DRAFT.with(|slot| {
                let slot = slot.borrow();
                let Some(cancel) = slot.as_ref().and_then(|draft| draft.cancel.as_ref()) else {
                    return;
                };
                cancel.store(true, Ordering::Relaxed);
                let sheet = app.global::<ReviewCopy>();
                sheet.set_cancelling(true);
                sheet.set_status(t("reviewCopy.cancelling").into());
            });
        })
    });
    sheet.on_close(|| {
        Shell::with(|_, app| {
            let sheet = app.global::<ReviewCopy>();
            if !sheet.get_busy() && !sheet.get_choosing() {
                close(&app);
            }
        })
    });
}

pub(crate) fn open(studio: &Studio) {
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let timeline = session.project().active();
    if timeline.duration() <= 0.0 {
        return;
    }
    Shell::with(|_, app| {
        let sheet = app.global::<ReviewCopy>();
        if sheet.get_open() {
            return;
        }
        DRAFT.with(|slot| {
            *slot.borrow_mut() = Some(Draft {
                epoch: host::project_epoch(),
                timeline: timeline.id.clone(),
                choosing: false,
                cancel: None,
            })
        });
        sheet.set_status("".into());
        sheet.set_error("".into());
        sheet.set_available(true);
        sheet.set_open(true);
    });
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let available = studio
        .session
        .as_ref()
        .is_some_and(|session| session.project().active().duration() > 0.0);
    app.global::<ReviewCopy>().set_available(available);
    let stale = DRAFT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|draft| !draft.current(studio))
    });
    if stale {
        close(app);
    }
}

fn close(app: &App) {
    DRAFT.with(|slot| *slot.borrow_mut() = None);
    let sheet = app.global::<ReviewCopy>();
    sheet.set_open(false);
    sheet.set_choosing(false);
    sheet.set_busy(false);
    sheet.set_cancelling(false);
}

fn choose_folder() {
    Shell::with(|shell, app| {
        let studio = shell.studio.borrow();
        let Some(session) = studio.session.as_ref() else {
            return;
        };
        let request = DRAFT.with(|slot| {
            let mut slot = slot.borrow_mut();
            let draft = slot.as_mut().filter(|draft| draft.current(&studio))?;
            if draft.choosing || draft.cancel.is_some() {
                return None;
            }
            draft.choosing = true;
            Some((draft.epoch, draft.timeline.clone()))
        });
        let Some((epoch, timeline)) = request else {
            return;
        };
        app.global::<ReviewCopy>().set_choosing(true);
        let title = t("reviewCopy.folder");
        let start = session.path().to_owned();
        host::after_dialog(
            move || Some(crate::platform::pick_folder(&title, &start)),
            move |studio, app, _, picked| {
                let chose = DRAFT.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    let Some(draft) = slot.as_mut().filter(|draft| {
                        draft.epoch == epoch && draft.timeline == timeline && draft.current(studio)
                    }) else {
                        return false;
                    };
                    draft.choosing = false;
                    true
                });
                if chose {
                    app.global::<ReviewCopy>().set_choosing(false);
                    if let Some(destination) = picked {
                        start_copy(studio, app, destination);
                    }
                }
            },
        );
    });
}

fn start_copy(studio: &mut Studio, app: &App, destination: PathBuf) {
    studio.flush_commit();
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    if session.project().active().duration() <= 0.0 {
        return;
    }
    let sheet = app.global::<ReviewCopy>();
    let job = match studio.host.exporter.begin() {
        Ok(job) => job,
        Err(_) => {
            sheet.set_error(t("reviewCopy.busy").into());
            return;
        }
    };
    let flag = job.cancel_handle();
    DRAFT.with(|slot| {
        if let Some(draft) = slot.borrow_mut().as_mut() {
            draft.cancel = Some(Arc::clone(&flag));
        }
    });
    sheet.set_error("".into());
    sheet.set_status(t("reviewCopy.preparing").into());
    sheet.set_cancelling(false);
    sheet.set_progress(0.0);
    sheet.set_busy(true);
    let epoch = host::project_epoch();
    let snapshot = Snapshot {
        project: session.project().clone(),
        project_dir: PathBuf::from(session.path()),
        name: concat_host::projects::folder_name(&studio.project_name) + "-review",
        titles: Arc::clone(&studio.host.titles),
        monitor: studio.host.monitor.clone(),
    };
    let updates = Arc::clone(&flag);
    host::spawn_long_in_project(
        move || {
            let mut last = ("", -1);
            let mut report = |frame: i64, total: i64, stage: &'static str| {
                let percent = if total > 0 {
                    (frame.saturating_mul(100) / total).clamp(0, 100)
                } else {
                    0
                };
                if (stage, percent) == last {
                    return;
                }
                last = (stage, percent);
                let ticket = Arc::clone(&updates);
                host::on_ui_in_project(epoch, move |_, app, _| {
                    if active(&ticket) && !ticket.load(Ordering::Relaxed) {
                        let ui = app.global::<ReviewCopy>();
                        let (label, progress) = stage_progress(stage, percent as f32 / 100.0);
                        ui.set_status(label.into());
                        ui.set_progress(progress);
                    }
                });
            };
            let result = snapshot.render(&destination, job.cancel_flag(), &mut report);
            if job.cancelled() && result.is_err() {
                Err(CopyError::Cancelled)
            } else {
                result
            }
        },
        move |studio, app, _, result| {
            if !active(&flag) {
                return;
            }
            DRAFT.with(|slot| {
                if let Some(draft) = slot.borrow_mut().as_mut() {
                    draft.cancel = None;
                }
            });
            let sheet = app.global::<ReviewCopy>();
            sheet.set_busy(false);
            sheet.set_cancelling(false);
            sheet.set_status("".into());
            match result {
                Ok(folder) => {
                    close(app);
                    studio.notify(&tf("reviewCopy.saved", &[&folder.display()]), false);
                }
                Err(CopyError::Cancelled) => sheet.set_status(t("reviewCopy.cancelled").into()),
                Err(error) => sheet.set_error(error.message().into()),
            }
        },
    );
}

fn active(flag: &Arc<AtomicBool>) -> bool {
    DRAFT.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|draft| draft.cancel.as_ref())
            .is_some_and(|held| Arc::ptr_eq(held, flag))
    })
}

fn stage_progress(stage: &'static str, fraction: f32) -> (String, f32) {
    match stage {
        "rendering" => (t("export.renderingVideo"), fraction * 0.88),
        concat_export::RENDERING_IN_SOFTWARE => (t("export.renderingInSoftware"), fraction * 0.88),
        "mixing audio" => (t("export.mixingAudio"), 0.90),
        "muxing" => (t("export.finalisingFile"), 0.97),
        _ => (t("reviewCopy.preparing"), 0.0),
    }
}

struct Snapshot {
    project: Project,
    project_dir: PathBuf,
    name: String,
    titles: Arc<Titles>,
    monitor: Monitor,
}

impl Snapshot {
    fn render(
        self,
        destination: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(i64, i64, &'static str),
    ) -> Result<PathBuf, CopyError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let timeline = self.project.active();
        let video = timeline.video;
        let (width, height) =
            size::dimensions(video.width, video.height).ok_or(CopyError::Dimensions)?;
        let notes =
            crate::agent::review_notes(&self.project_dir, timeline).map_err(CopyError::Notes)?;
        let output = OutputSet::new(destination, &[VIDEO, NOTES]).map_err(CopyError::Storage)?;
        std::fs::write(output.path().join(NOTES), notes).map_err(CopyError::Storage)?;
        let titles = self
            .titles
            .clips_unless(&self.project, width, height, cancel)
            .ok_or(CopyError::Cancelled)?
            .into_iter()
            .map(|title| title.clip)
            .collect();
        let spec = ExportSpec {
            output: output.path().join(VIDEO).to_string_lossy().into_owned(),
            crf: 26,
            preset: "fast".to_owned(),
            codec: VideoCodec::H264,
            ..ExportSpec::default()
        };
        let mut request =
            concat_host::export::request_for(&self.project, &self.project_dir, &spec, titles);
        request.width = width;
        request.height = height;
        render_review_on(
            &request,
            self.monitor.off_thread(),
            Reporter { progress, cancel },
        )
        .map_err(CopyError::Render)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        output.publish(&self.name).map_err(CopyError::Storage)
    }
}

#[derive(Debug)]
enum CopyError {
    Cancelled,
    Dimensions,
    Notes(String),
    Storage(std::io::Error),
    Render(String),
}

impl CopyError {
    fn message(&self) -> String {
        match self {
            Self::Cancelled => t("reviewCopy.cancelled"),
            Self::Dimensions => t("reviewCopy.dimensions"),
            Self::Notes(detail) => {
                log::warn!("Review copy notes failed: {detail}");
                t("reviewCopy.notesError")
            }
            Self::Storage(detail) => {
                log::warn!("Review copy write failed: {detail}");
                t("reviewCopy.storageError")
            }
            Self::Render(detail) => {
                log::warn!("Review copy render failed: {detail}");
                t("reviewCopy.renderError")
            }
        }
    }
}
