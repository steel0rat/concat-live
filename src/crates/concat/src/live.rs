// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Synchronous API calls cross to the window's event loop, never to a second editor.

use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

use concat_api::{ApiError, Done, ErrorCode, LiveHandler, Reply, Request};
use concat_host::{Session, projects};
use concat_project::Command;

use crate::host::{self, Shell};
use crate::studio::{Gesture, Studio};

const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const CANCELLED: u8 = 2;
const UI_TIMEOUT: Duration = Duration::from_secs(45);

static SERVER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Revoke queued work before an Automation server is stopped or replaced.
pub(crate) fn invalidate_server() {
    SERVER_EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// The bridge is bound to the server that accepted the request.
pub(crate) struct Window {
    generation: u64,
}

impl Window {
    pub(crate) fn new() -> Self {
        Self {
            generation: SERVER_EPOCH.load(Ordering::Acquire),
        }
    }
}

/// A snapshot and its order are captured together before a worker writes it.
pub(crate) struct SaveSnapshot {
    pub path: String,
    pub document: serde_json::Value,
    pub ticket: projects::Ticket,
    pub revision: u64,
    pub epoch: u64,
}

impl LiveHandler for Window {
    fn call(&self, request: Request) -> Result<Reply, ApiError> {
        if let Request::ProjectSave { path, name } = request {
            let snapshot = on_window(self.generation, move |studio| {
                require_path(studio, &path)?;
                studio
                    .live_prepare_save(name.as_deref())
                    .map_err(ApiError::failed)
            })?;
            let result =
                projects::save_ordered(&snapshot.path, &snapshot.document, snapshot.ticket)
                    .map(|_| ());
            let notice = result.clone();
            host::on_ui_in_project(snapshot.epoch, move |studio, _, _| {
                studio.live_saved(snapshot.revision, notice);
            });
            result.map_err(ApiError::failed)?;
            return Ok(Reply::Done(Done {}));
        }
        on_window(self.generation, move |studio| dispatch(studio, request))
    }

    fn snapshot(&self, path: &str) -> Result<Session, ApiError> {
        let path = path.to_owned();
        on_window(self.generation, move |studio| {
            require_path(studio, &path)?;
            Ok(session(studio)?.snapshot())
        })
    }
}

/// A timed-out queued request is cancelled before it can mutate the window.
/// A request already executing has an uncertain outcome and must not be retried.
fn on_window<T: Send + 'static>(
    generation: u64,
    action: impl FnOnce(&mut Studio) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let epoch = host::project_epoch();
    let state = Arc::new(AtomicU8::new(QUEUED));
    let entered = Arc::clone(&state);
    let (sender, receiver) = mpsc::sync_channel(1);
    slint::invoke_from_event_loop(move || {
        if entered
            .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let mut sender = Some(sender);
        Shell::with(|shell, app| {
            let result = match shell.studio.try_borrow_mut() {
                Ok(mut studio) => gate(&studio, epoch, generation).and_then(|()| {
                    studio.flush_commit();
                    action(&mut studio)
                }),
                Err(_) => Err(ApiError::new(
                    ErrorCode::Busy,
                    "the window is handling another edit",
                )),
            };
            if let Ok(mut studio) = shell.studio.try_borrow_mut() {
                studio.refresh_art();
            }
            if let Ok(studio) = shell.studio.try_borrow() {
                studio.publish(&app, &shell.models);
            }
            if let Some(sender) = sender.take() {
                let _ = sender.send(result);
            }
        });
        if let Some(sender) = sender {
            let _ = sender.send(Err(ApiError::new(
                ErrorCode::NotOpen,
                "the Concat window is closed",
            )));
        }
    })
    .map_err(|error| ApiError::failed(format!("could not reach the window: {error}")))?;
    receive(receiver, &state)
}

fn receive<T>(
    receiver: mpsc::Receiver<Result<T, ApiError>>,
    state: &AtomicU8,
) -> Result<T, ApiError> {
    match receiver.recv_timeout(UI_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(ApiError::failed("the window closed before replying"))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let cancelled = state
                .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            let message = if cancelled {
                "the window did not accept the request in time; the queued request was cancelled"
            } else {
                "the window did not reply in time; outcome unknown, inspect the project before retrying"
            };
            Err(ApiError::new(ErrorCode::Busy, message))
        }
    }
}

fn gate(studio: &Studio, epoch: u64, generation: u64) -> Result<(), ApiError> {
    if generation != SERVER_EPOCH.load(Ordering::Acquire) {
        return Err(ApiError::new(
            ErrorCode::Refused,
            "the Automation server changed before this request reached the window",
        ));
    }
    if !studio.live_enabled || studio.host.server.is_none() {
        return Err(ApiError::new(
            ErrorCode::Refused,
            "live automation is disabled",
        ));
    }
    if epoch != host::project_epoch() {
        return Err(ApiError::new(
            ErrorCode::Busy,
            "the visible project changed while the request was queued",
        ));
    }
    if !matches!(&studio.gesture, Gesture::None) || studio.confirm.is_some() {
        return Err(ApiError::new(
            ErrorCode::Busy,
            "finish the current gesture or confirmation before editing from the agent",
        ));
    }
    Ok(())
}

fn session(studio: &Studio) -> Result<&Session, ApiError> {
    studio
        .session
        .as_ref()
        .ok_or_else(|| ApiError::new(ErrorCode::NotOpen, "there is no project in the live window"))
}

fn require_path(studio: &Studio, path: &str) -> Result<(), ApiError> {
    if path.is_empty() || !Path::new(path).is_absolute() {
        return Err(ApiError::invalid(
            "live calls require the absolute visible project directory",
        ));
    }
    let active = session(studio)?;
    let matches = Path::new(path)
        .canonicalize()
        .ok()
        .zip(Path::new(active.path()).canonicalize().ok())
        .is_some_and(|(requested, visible)| requested == visible);
    if !matches {
        return Err(ApiError::new(
            ErrorCode::NotOpen,
            "the requested project is not the one visible in Concat; open it explicitly first",
        ));
    }
    Ok(())
}

fn can_switch(studio: &Studio, path: Option<&str>) -> Result<(), ApiError> {
    if let Some(path) = path
        && require_path(studio, path).is_ok()
    {
        return Ok(());
    }
    if studio.live_is_dirty() {
        return Err(ApiError::new(
            ErrorCode::Refused,
            "save the visible project's unsaved edits before opening another project",
        ));
    }
    Ok(())
}

fn view(studio: &Studio) -> Result<Reply, ApiError> {
    Ok(Reply::View(Box::new(session(studio)?.view())))
}

fn dispatch(studio: &mut Studio, request: Request) -> Result<Reply, ApiError> {
    match request {
        Request::ProjectCreate {
            location,
            name,
            video,
        } => {
            can_switch(studio, None)?;
            let video = video.unwrap_or_default();
            let info = projects::create(
                &location,
                &name,
                video.width,
                video.height,
                video.rate_num,
                video.rate_den,
                video.canvas,
            )
            .map_err(ApiError::failed)?;
            studio.open_project(info).map_err(ApiError::failed)?;
            view(studio)
        }
        Request::ProjectOpen { path } => {
            can_switch(studio, Some(&path))?;
            let info = projects::open(&path).map_err(ApiError::failed)?;
            studio.open_project(info).map_err(ApiError::failed)?;
            view(studio)
        }
        Request::ProjectGet { path } => {
            require_path(studio, &path)?;
            view(studio)
        }
        Request::ProjectDocument { path } => {
            require_path(studio, &path)?;
            Ok(Reply::Document(session(studio)?.document()))
        }
        Request::ProjectClose { path, save } => {
            require_path(studio, &path)?;
            studio
                .close_live_project(save)
                .map_err(|error| ApiError::new(ErrorCode::Refused, error))?;
            Ok(Reply::Done(Done {}))
        }
        Request::ProjectSetVideo { path, video } => {
            require_path(studio, &path)?;
            let command = Command::SetTimelineVideo {
                timeline_id: session(studio)?.project().active_timeline_id.clone(),
                video,
            };
            studio
                .try_apply(command)
                .map(|view| Reply::View(Box::new(view)))
                .map_err(|error| ApiError::new(ErrorCode::Refused, error))
        }
        Request::EditApply { path, command } => {
            require_path(studio, &path)?;
            studio
                .try_apply(*command)
                .map(|view| Reply::View(Box::new(view)))
                .map_err(|error| ApiError::new(ErrorCode::Refused, error))
        }
        Request::EditUndo { path } => {
            require_path(studio, &path)?;
            studio.undo();
            view(studio)
        }
        Request::EditRedo { path } => {
            require_path(studio, &path)?;
            studio.redo();
            view(studio)
        }
        _ => Err(ApiError::invalid(
            "this request does not belong to the live UI dispatcher",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_server_revokes_the_previous_handler() {
        let first = Window::new();
        let same_server = Window::new();
        assert_eq!(first.generation, same_server.generation);
        invalidate_server();
        let replacement = Window::new();
        assert_ne!(first.generation, replacement.generation);
        assert_eq!(replacement.generation, SERVER_EPOCH.load(Ordering::Acquire));
    }

    #[test]
    fn disconnected_ui_is_an_error_instead_of_a_success() {
        let (sender, receiver) = mpsc::sync_channel::<Result<(), ApiError>>(1);
        drop(sender);
        let error = receive(receiver, &AtomicU8::new(QUEUED)).unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
    }

    #[test]
    fn completed_ui_refusal_is_preserved() {
        let (sender, receiver) = mpsc::sync_channel::<Result<(), ApiError>>(1);
        sender
            .send(Err(ApiError::new(ErrorCode::NotOpen, "wrong project")))
            .unwrap();
        let error = receive(receiver, &AtomicU8::new(RUNNING)).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotOpen);
        assert_eq!(error.message, "wrong project");
    }
}
