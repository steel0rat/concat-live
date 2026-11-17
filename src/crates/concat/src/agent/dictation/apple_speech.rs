// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! A cancellable, in-process Apple Speech session. Swift owns the device
//! and tasks; Rust owns delivery to the pane, identified without raw contexts.

use std::sync::mpsc::{Receiver, TryRecvError};

#[cfg(target_os = "macos")]
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
};

#[derive(Debug, PartialEq)]
pub(super) enum Event {
    Status(String),
    Recording,
    Finalizing,
    Transcript(String),
    Meter { level: f32, seconds: f32 },
    Finished(String),
    Failed(String),
}

pub(super) struct Session {
    #[cfg(target_os = "macos")]
    id: u64,
    events: Receiver<Event>,
}

pub(super) fn available() -> bool {
    #[cfg(target_os = "macos")]
    {
        // This only checks OS availability; it never opens the microphone.
        unsafe { concat_dictation_available() != 0 }
    }
    #[cfg(not(target_os = "macos"))]
    false
}

impl Session {
    pub(super) fn start(locale: &str) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            if !available() {
                return Err("Для локальной диктовки Apple нужна macOS 26 или новее.".into());
            }
            let locale = CString::new(locale).map_err(|_| "Некорректный язык диктовки.")?;
            let id = NEXT_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| "Не удалось создать новую сессию диктовки.")?;
            let (sender, events) = mpsc::channel();
            registry().insert(id, sender);
            // Swift copies the locale before returning and only retains the
            // numeric ID. The callback clones its text before returning.
            unsafe { concat_dictation_start(id, locale.as_ptr(), receive) };
            Ok(Self { id, events })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = locale;
            Err("Локальная диктовка Apple доступна только на macOS 26 или новее.".into())
        }
    }

    #[cfg(all(target_os = "macos", concat_dictation_qa))]
    pub(super) fn start_file(locale: &str, path: &std::path::Path) -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt;
        if !available() {
            return Err("Для локальной диктовки Apple нужна macOS 26 или новее.".into());
        }
        let locale = CString::new(locale).map_err(|_| "Некорректный язык диктовки.")?;
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| "Некорректный путь записи.")?;
        let id = NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| "Не удалось создать сессию проверки диктовки.")?;
        let (sender, events) = mpsc::channel();
        registry().insert(id, sender);
        unsafe { concat_dictation_start_file(id, locale.as_ptr(), path.as_ptr(), receive) };
        Ok(Self { id, events })
    }

    pub(super) fn stop(&self) {
        #[cfg(target_os = "macos")]
        unsafe {
            concat_dictation_stop(self.id);
        }
    }

    pub(super) fn cancel(&self) {
        #[cfg(target_os = "macos")]
        {
            // Unregister first: cancellation may race with an in-flight result.
            registry().remove(&self.id);
            unsafe { concat_dictation_cancel(self.id) };
        }
    }

    pub(super) fn try_recv(&self) -> Result<Event, TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(target_os = "macos")]
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
static REGISTRY: OnceLock<Mutex<HashMap<u64, Sender<Event>>>> = OnceLock::new();

#[cfg(target_os = "macos")]
fn registry() -> std::sync::MutexGuard<'static, HashMap<u64, Sender<Event>>> {
    REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(target_os = "macos")]
extern "C" fn receive(id: u64, kind: i32, text: *const c_char, level: f32, seconds: f64) {
    let text = if text.is_null() {
        String::new()
    } else {
        // Swift's withCString keeps this pointer valid for this call only.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    };
    let event = match kind {
        0 => Event::Status(text),
        1 => Event::Recording,
        2 => Event::Transcript(text),
        3 => Event::Meter {
            level: if level.is_finite() {
                level.clamp(0.0, 1.0)
            } else {
                0.0
            },
            seconds: if seconds.is_finite() {
                seconds.clamp(0.0, f64::from(f32::MAX)) as f32
            } else {
                0.0
            },
        },
        4 => Event::Finished(text),
        5 => Event::Failed(text),
        6 => Event::Finalizing,
        _ => Event::Failed("Нативная диктовка прислала неизвестное состояние.".into()),
    };
    let terminal = matches!(event, Event::Finished(_) | Event::Failed(_));
    let sender = {
        let mut entries = registry();
        if terminal {
            entries.remove(&id)
        } else {
            entries.get(&id).cloned()
        }
    };
    if let Some(sender) = sender
        && sender.send(event).is_err()
    {
        registry().remove(&id);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn concat_dictation_available() -> i32;
    fn concat_dictation_start(
        id: u64,
        locale: *const c_char,
        callback: extern "C" fn(u64, i32, *const c_char, f32, f64),
    );
    #[cfg(concat_dictation_qa)]
    fn concat_dictation_start_file(
        id: u64,
        locale: *const c_char,
        path: *const c_char,
        callback: extern "C" fn(u64, i32, *const c_char, f32, f64),
    );
    fn concat_dictation_stop(id: u64);
    fn concat_dictation_cancel(id: u64);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn session() -> Session {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, events) = mpsc::channel();
        registry().insert(id, sender);
        Session { id, events }
    }

    #[test]
    fn text_is_copied_before_the_native_callback_returns() {
        let session = session();
        let text = CString::new("Создай ролик про ёжика").expect("valid text");
        receive(session.id, 2, text.as_ptr(), 0.0, 0.0);
        drop(text);
        assert_eq!(
            session.try_recv(),
            Ok(Event::Transcript("Создай ролик про ёжика".into()))
        );
    }

    #[test]
    fn terminal_result_unregisters_before_late_callbacks() {
        let session = session();
        let text = CString::new("готово").expect("valid text");
        receive(session.id, 4, text.as_ptr(), 0.0, 0.0);
        receive(session.id, 2, text.as_ptr(), 0.0, 0.0);
        assert_eq!(session.try_recv(), Ok(Event::Finished("готово".into())));
        assert_eq!(session.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn cancelled_owner_cannot_receive_late_results() {
        let session = session();
        let id = session.id;
        session.cancel();
        let text = CString::new("late").expect("valid text");
        receive(id, 4, text.as_ptr(), 0.0, 0.0);
        assert_eq!(session.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn malformed_native_meter_values_do_not_reach_layout() {
        let session = session();
        receive(session.id, 3, std::ptr::null(), f32::NAN, f64::INFINITY);
        assert_eq!(
            session.try_recv(),
            Ok(Event::Meter {
                level: 0.0,
                seconds: 0.0
            })
        );
    }
}
