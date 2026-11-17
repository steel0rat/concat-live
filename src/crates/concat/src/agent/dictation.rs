// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

mod apple_speech;
mod state;

use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use crate::ui::Agent;
use apple_speech::{Event, Session};
use state::State;

pub(super) use state::append_transcript;

#[derive(Default)]
pub(super) struct Dictation {
    state: State,
    session: Option<Session>,
}

impl Dictation {
    pub fn active(&self) -> bool {
        self.state.active()
    }

    pub fn start(&mut self, locale: &str) {
        if !self.state.begin() {
            return;
        }
        match Session::start(locale) {
            Ok(session) => self.session = Some(session),
            Err(error) => self.state.fail(error),
        }
    }

    pub fn stop(&mut self) {
        if self.state.stopping(Instant::now())
            && let Some(session) = &self.session
        {
            session.stop();
        }
    }

    pub fn cancel(&mut self) {
        if let Some(session) = self.session.take() {
            session.cancel();
        }
        self.state.cancel();
    }

    pub fn poll(&mut self) -> Option<String> {
        for _ in 0..256 {
            let Some(session) = &self.session else { break };
            match session.try_recv() {
                Ok(Event::Status(text)) => self.state.status(text),
                Ok(Event::Recording) => self.state.recording(),
                Ok(Event::Finalizing) => {
                    self.state.stopping(Instant::now());
                }
                Ok(Event::Transcript(text)) => self.state.transcript(text),
                Ok(Event::Meter { level, seconds }) => {
                    if self.state.meter(level, seconds) {
                        self.stop();
                    }
                }
                Ok(Event::Finished(text)) => {
                    self.session.take();
                    return self.state.finish(text);
                }
                Ok(Event::Failed(error)) => {
                    self.session.take();
                    self.state.fail(error);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.session.take();
                    self.state
                        .fail("Распознавание неожиданно остановилось. Попробуйте ещё раз.".into());
                    break;
                }
            }
        }
        if self.state.timed_out(Instant::now()) {
            self.session.take();
            self.state.fail("Распознавание не завершилось за 15 секунд. Запись остановлена; попробуйте ещё раз.".into());
        }
        None
    }

    pub fn publish(&self, global: &Agent) {
        global.set_dictation_available(apple_speech::available());
        global.set_dictation_phase(self.state.phase as i32);
        global.set_dictation_status(self.state.status.as_str().into());
        global.set_dictation_preview(self.state.preview.as_str().into());
        global.set_dictation_error(self.state.error.as_str().into());
        global.set_dictation_level(self.state.level);
        global.set_dictation_seconds(self.state.seconds);
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.cancel();
    }
}
