// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

use std::time::{Duration, Instant};

pub(super) const MAX_SECONDS: f32 = 180.0;
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(i32)]
pub(super) enum Phase {
    #[default]
    Idle = 0,
    Preparing = 1,
    Recording = 2,
    Finishing = 3,
}

#[derive(Default)]
pub(super) struct State {
    pub phase: Phase,
    pub status: String,
    pub preview: String,
    pub error: String,
    pub level: f32,
    pub seconds: i32,
    finishing_since: Option<Instant>,
}

impl State {
    pub fn active(&self) -> bool {
        self.phase != Phase::Idle
    }

    pub fn begin(&mut self) -> bool {
        if self.active() {
            return false;
        }
        *self = Self {
            phase: Phase::Preparing,
            status: "Подготовка микрофона…".into(),
            ..Self::default()
        };
        true
    }

    pub fn recording(&mut self) {
        if self.phase == Phase::Preparing {
            self.phase = Phase::Recording;
            self.status = "Говорите. После остановки текст будет добавлен в черновик.".into();
        }
    }

    pub fn status(&mut self, text: String) {
        if self.active() {
            self.status = text;
        }
    }

    pub fn transcript(&mut self, text: String) {
        if self.active() {
            self.preview = text;
        }
    }

    pub fn meter(&mut self, level: f32, seconds: f32) -> bool {
        if !self.active() {
            return false;
        }
        self.level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if seconds.is_finite() {
            self.seconds = seconds.clamp(0.0, MAX_SECONDS) as i32;
        }
        self.phase == Phase::Recording && seconds >= MAX_SECONDS
    }

    pub fn stopping(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Recording {
            return false;
        }
        self.phase = Phase::Finishing;
        self.status = "Завершение распознавания…".into();
        self.level = 0.0;
        self.finishing_since = Some(now);
        true
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        self.finishing_since
            .is_some_and(|since| now.saturating_duration_since(since) >= FINISH_TIMEOUT)
    }

    pub fn finish(&mut self, text: String) -> Option<String> {
        if !self.active() {
            return None;
        }
        let text = text.trim();
        if text.is_empty() {
            self.fail("Речь не распознана. Попробуйте ещё раз.".into());
            return None;
        }
        *self = Self::default();
        Some(text.to_owned())
    }

    pub fn fail(&mut self, error: String) {
        self.phase = Phase::Idle;
        self.status.clear();
        self.error = error;
        self.level = 0.0;
        self.finishing_since = None;
    }

    pub fn cancel(&mut self) {
        *self = Self::default();
    }
}

pub(crate) fn append_transcript(draft: &str, transcript: &str) -> String {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return draft.to_owned();
    }
    let separator = if draft.is_empty() || draft.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    format!("{draft}{separator}{transcript}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictated_text_is_committed_once_after_finishing() {
        let mut state = State::default();
        assert!(state.begin());
        state.recording();
        state.transcript("Создай".into());
        state.transcript("Создай вертикальный ролик".into());
        assert!(state.stopping(Instant::now()));
        assert_eq!(state.phase, Phase::Finishing);
        assert_eq!(
            state
                .finish(" Создай вертикальный ролик. ".into())
                .as_deref(),
            Some("Создай вертикальный ролик.")
        );
        assert_eq!(state.phase, Phase::Idle);
        assert_eq!(state.finish("duplicate late result".into()), None);
        assert!(state.preview.is_empty());
    }

    #[test]
    fn cancellation_never_commits_a_late_result() {
        let mut state = State::default();
        assert!(state.begin());
        state.recording();
        state.transcript("Не отправляй".into());
        state.cancel();
        state.transcript("late partial".into());
        state.recording();
        assert_eq!(state.finish("late final".into()), None);
        assert_eq!(state.phase, Phase::Idle);
        assert!(state.preview.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn finishing_does_not_turn_back_into_recording() {
        let mut state = State::default();
        state.begin();
        state.recording();
        let now = Instant::now();
        assert!(state.stopping(now));
        state.recording();
        assert_eq!(state.phase, Phase::Finishing);
        assert!(!state.stopping(now));
        assert!(!state.timed_out(now + Duration::from_secs(14)));
        assert!(state.timed_out(now + Duration::from_secs(15)));
        state.fail("Распознавание не завершилось.".into());
        assert!(!state.timed_out(now + Duration::from_secs(60)));
    }

    #[test]
    fn a_second_start_preserves_the_active_recording() {
        let mut state = State::default();
        state.begin();
        state.recording();
        state.transcript("Текущая речь".into());
        assert!(!state.begin());
        assert_eq!(state.phase, Phase::Recording);
        assert_eq!(state.preview, "Текущая речь");
    }

    #[test]
    fn errors_keep_the_partial_transcript_available_for_copying() {
        let mut state = State::default();
        state.begin();
        state.recording();
        state.transcript("Уже распознанный текст".into());
        state.fail("Микрофон отключён.".into());
        assert!(!state.active());
        assert_eq!(state.preview, "Уже распознанный текст");
        assert_eq!(state.error, "Микрофон отключён.");
        assert_eq!(state.finish("late final".into()), None);
        assert!(state.begin());
        assert!(state.preview.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn empty_recognition_does_not_replace_the_draft() {
        let mut state = State::default();
        state.begin();
        state.recording();
        assert_eq!(state.finish(" \n ".into()), None);
        assert!(!state.error.is_empty());
        assert_eq!(
            append_transcript("Сохранённый черновик", " \n "),
            "Сохранённый черновик"
        );
    }

    #[test]
    fn final_text_preserves_edits_typed_while_recording() {
        assert_eq!(
            append_transcript("Набрано во время записи", "голосом"),
            "Набрано во время записи голосом"
        );
        assert_eq!(
            append_transcript("Заголовок:\n", "Привет"),
            "Заголовок:\nПривет"
        );
        assert_eq!(append_transcript("", " Привет! "), "Привет!");
        assert_eq!(append_transcript("🙂 ", "Жизнь"), "🙂 Жизнь");
    }

    #[test]
    fn the_meter_is_bounded_and_requests_a_stop_at_three_minutes() {
        let mut state = State::default();
        state.begin();
        state.recording();
        assert!(!state.meter(f32::NAN, f32::NAN));
        assert_eq!(state.level, 0.0);
        assert!(!state.meter(2.0, -3.0));
        assert_eq!(state.level, 1.0);
        assert_eq!(state.seconds, 0);
        assert!(!state.meter(0.25, 179.9));
        assert!(state.meter(0.25, 180.0));
        state.stopping(Instant::now());
        assert!(!state.meter(0.25, 200.0));
        assert_eq!(state.seconds, 180);
        state.cancel();
        assert!(!state.meter(1.0, 200.0));
        assert_eq!(state.level, 0.0);
    }
}
