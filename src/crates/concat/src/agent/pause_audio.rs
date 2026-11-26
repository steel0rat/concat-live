// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Audio analysis and audition for pause review, on detached editor snapshots.

use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use concat_export::{ClipKind, audio_buses, audio_pieces};
use concat_host::session::Session;
use concat_media::audio::{self, AudioBus, AudioClip, MIX_RATE};

use crate::i18n::{t, tf};

const BLOCK_FRAMES: u64 = MIX_RATE as u64 / 50;
const MIN_PAUSE_FRAMES: u64 = MIX_RATE as u64 * 2 / 5;
const EDGE_FRAMES: u64 = MIX_RATE as u64 / 10;
const RMS_MAX: u32 = 520;
const PEAK_MAX: u32 = 1_024;
const MAX_PAUSES: usize = 64;
const JOIN_MARGIN: f64 = 0.8;

struct TimelineAudio {
    clips: Vec<AudioClip>,
    buses: Vec<AudioBus>,
}

impl TimelineAudio {
    fn of(snapshot: &Session) -> Self {
        let clips = snapshot.flattened_clips();
        Self {
            clips: clips
                .iter()
                .filter(|clip| {
                    !clip.muted
                        && matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
                        && clip.has_audio == Some(true)
                })
                .flat_map(audio_pieces)
                .collect(),
            buses: audio_buses(&clips),
        }
    }

    fn scan(
        &self,
        from: f64,
        to: f64,
        cancelled: &AtomicBool,
        mut feed: impl FnMut(&[u8]) -> bool,
    ) -> Result<(), String> {
        active(cancelled)?;
        let mut stopped = false;
        let result = audio::mix_pcm(&self.clips, &self.buses, from, to, |bytes| {
            stopped = cancelled.load(Ordering::Relaxed) || !feed(bytes);
            if stopped {
                return Err(concat_media::Error::Io {
                    path: PathBuf::from("timeline audio"),
                    source: io::ErrorKind::Interrupted.into(),
                });
            }
            Ok(())
        });
        active(cancelled)?;
        if stopped {
            return Ok(());
        }
        result.map_err(|error| tf("chatUi.pauseReadAudioFailed", &[&error]))
    }
}

fn active(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(t("chatUi.pauseAudioCancelled"));
    }
    Ok(())
}

fn stereo_frames(bytes: &[u8]) -> impl Iterator<Item = [i16; 2]> + '_ {
    bytes.chunks_exact(4).map(|frame| {
        [
            i16::from_ne_bytes([frame[0], frame[1]]),
            i16::from_ne_bytes([frame[2], frame[3]]),
        ]
    })
}

/// Keeps only quiet runs between audible blocks. A silent clip or a quiet
/// tail at the boundary of In/Out provides no transition to review.
struct PauseScan {
    from: f64,
    frames: u64,
    energy: u64,
    peak: u32,
    heard: bool,
    quiet_start: Option<u64>,
}

impl PauseScan {
    fn new(from: f64) -> Self {
        Self {
            from,
            frames: 0,
            energy: 0,
            peak: 0,
            heard: false,
            quiet_start: None,
        }
    }

    fn add(&mut self, stereo: [i16; 2]) -> Option<[f64; 2]> {
        let magnitude = u32::from(stereo[0].unsigned_abs().max(stereo[1].unsigned_abs()));
        self.peak = self.peak.max(magnitude);
        self.energy += u64::from(magnitude * magnitude);
        self.frames += 1;
        if !self.frames.is_multiple_of(BLOCK_FRAMES) {
            return None;
        }
        let at = self.frames - BLOCK_FRAMES;
        let quiet =
            self.peak <= PEAK_MAX && self.energy <= u64::from(RMS_MAX * RMS_MAX) * BLOCK_FRAMES;
        self.peak = 0;
        self.energy = 0;
        if quiet {
            if self.heard {
                self.quiet_start.get_or_insert(at);
            }
            return None;
        }

        self.heard = true;
        let start = self.quiet_start.take()?;
        if at - start < MIN_PAUSE_FRAMES {
            return None;
        }
        let seconds = |frames| self.from + frames as f64 / f64::from(MIX_RATE);
        Some([seconds(start + EDGE_FRAMES), seconds(at - EDGE_FRAMES)])
    }
}

/// Find up to 64 pause cuts in the supplied windows, using the sound heard
/// in the editor, including lane mute, clip gain, retiming and sound layers.
pub(super) fn detect_pauses(
    snapshot: &Session,
    windows: &[[f64; 2]],
    cancelled: &AtomicBool,
) -> Result<Vec<[f64; 2]>, String> {
    active(cancelled)?;
    let audio = TimelineAudio::of(snapshot);
    let mut pauses = Vec::new();
    for &[from, to] in windows {
        if !from.is_finite() || !to.is_finite() || from < 0.0 || from >= to {
            return Err(t("chatUi.pauseInvalidRange"));
        }
        let mut scan = PauseScan::new(from);
        audio.scan(from, to, cancelled, |bytes| {
            for frame in stereo_frames(bytes) {
                if let Some(pause) = scan.add(frame) {
                    pauses.push(pause);
                    if pauses.len() == MAX_PAUSES {
                        return false;
                    }
                }
            }
            true
        })?;
        if pauses.len() == MAX_PAUSES {
            break;
        }
    }
    Ok(pauses)
}

/// Mix the surrounding audio of a join in the already edited snapshot.
pub(super) fn render_join(
    snapshot: &Session,
    at: f64,
    cancelled: &AtomicBool,
) -> Result<Arc<[[i16; 2]]>, String> {
    active(cancelled)?;
    let duration = snapshot.project().active().duration();
    if !at.is_finite() || at <= 0.0 || at >= duration {
        return Err(t("chatUi.pauseOutsideTimeline"));
    }
    let from = (at - JOIN_MARGIN).max(0.0);
    let to = (at + JOIN_MARGIN).min(duration);
    let mut samples = Vec::with_capacity(((to - from) * f64::from(MIX_RATE)).ceil() as usize);
    TimelineAudio::of(snapshot).scan(from, to, cancelled, |bytes| {
        samples.extend(stereo_frames(bytes));
        true
    })?;
    if samples.is_empty() {
        return Err(t("chatUi.pauseNoAudioToPlay"));
    }
    Ok(samples.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(detector: &mut PauseScan, blocks: u64, samples: [i16; 2]) -> Vec<[f64; 2]> {
        (0..blocks * BLOCK_FRAMES)
            .filter_map(|_| detector.add(samples))
            .collect()
    }

    #[test]
    fn finds_inner_pauses_with_padding_at_the_timeline_offset() {
        let mut scan = PauseScan::new(5.0);
        assert!(push(&mut scan, 25, [0, 0]).is_empty());
        assert!(push(&mut scan, 1, [0, 4_096]).is_empty());
        assert!(push(&mut scan, 20, [0, 0]).is_empty());
        let cuts = push(&mut scan, 1, [4_096, 0]);
        assert_eq!(cuts.len(), 1);
        assert!((cuts[0][0] - 5.62).abs() < 1e-8);
        assert!((cuts[0][1] - 5.82).abs() < 1e-8);
        assert!(push(&mut scan, 25, [0, 0]).is_empty());
    }

    #[test]
    fn short_pauses_and_signal_on_only_one_channel_do_not_get_cut() {
        let mut scan = PauseScan::new(0.0);
        assert!(push(&mut scan, 1, [4_096, 0]).is_empty());
        assert!(push(&mut scan, 10, [0, 0]).is_empty());
        assert!(push(&mut scan, 30, [0, -4_096]).is_empty());
    }
}
