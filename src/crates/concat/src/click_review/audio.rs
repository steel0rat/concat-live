// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_export::{ClipKind, ExportClip, audio_buses, audio_pieces};
use concat_media::audio::{self, AudioBus, AudioClip, MIX_RATE};

use super::model::{Join, discontinuity};

const LOOK_AROUND: f64 = 0.06;
const AUDITION_AROUND: f64 = 0.8;

#[derive(Debug)]
pub(super) enum ScanError {
    Cancelled,
    Mix(concat_media::Error),
}

pub(super) struct Detected {
    pub join: usize,
    pub sample_step: u32,
}

struct TimelineMix {
    pieces: Vec<(usize, AudioClip)>,
    buses: Vec<AudioBus>,
}

impl TimelineMix {
    fn of(clips: &[ExportClip]) -> Self {
        Self {
            pieces: clips
                .iter()
                .filter(|clip| {
                    !clip.muted
                        && clip.has_audio == Some(true)
                        && matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
                })
                .flat_map(|clip| {
                    audio_pieces(clip)
                        .into_iter()
                        .map(|piece| (clip.track, piece))
                })
                .collect(),
            buses: audio_buses(clips),
        }
    }

    fn render(
        &self,
        track: Option<usize>,
        from: f64,
        to: f64,
        cancelled: &AtomicBool,
    ) -> Result<Vec<[i16; 2]>, ScanError> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        let members: Vec<_> = self
            .pieces
            .iter()
            .filter(|(lane, piece)| {
                track.is_none_or(|track| track == *lane)
                    && from < piece.start + piece.duration
                    && piece.start < to
            })
            .map(|(_, piece)| piece.clone())
            .collect();
        if members.is_empty() || to <= from {
            return Ok(Vec::new());
        }
        let mut samples = Vec::with_capacity(((to - from) * f64::from(MIX_RATE)).ceil() as usize);
        let result = audio::mix_pcm(&members, &self.buses, from, to, |bytes| {
            if cancelled.load(Ordering::Relaxed) {
                return Err(concat_media::Error::Io {
                    path: PathBuf::from("timeline audio"),
                    source: io::ErrorKind::Interrupted.into(),
                });
            }
            samples.extend(bytes.chunks_exact(4).map(|frame| {
                [
                    i16::from_ne_bytes([frame[0], frame[1]]),
                    i16::from_ne_bytes([frame[2], frame[3]]),
                ]
            }));
            Ok(())
        });
        if cancelled.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        result.map_err(ScanError::Mix)?;
        Ok(samples)
    }
}

pub(super) fn detect(
    clips: &[ExportClip],
    duration: f64,
    joins: &[Join],
    cancelled: &AtomicBool,
) -> Result<Vec<Detected>, ScanError> {
    let mix = TimelineMix::of(clips);
    let mut detected = Vec::new();
    for (index, join) in joins.iter().enumerate() {
        // The mixer positions inputs with whole-millisecond delay. An origin
        // a whole number of milliseconds before the join keeps that delay exact.
        let lead_ms = (join.at * 1000.0).floor().clamp(0.0, LOOK_AROUND * 1000.0);
        let from = (join.at - lead_ms / 1000.0).max(0.0);
        let to = (join.at + LOOK_AROUND).min(duration);
        let samples = mix.render(Some(join.track), from, to, cancelled)?;
        let at = ((join.at - from) * f64::from(MIX_RATE)).round() as usize;
        if let Some(sample_step) = discontinuity(&samples, at) {
            detected.push(Detected {
                join: index,
                sample_step,
            });
        }
    }
    Ok(detected)
}

pub(super) fn render_audition(
    clips: &[ExportClip],
    duration: f64,
    at: f64,
    cancelled: &AtomicBool,
) -> Result<Vec<[i16; 2]>, ScanError> {
    let from = (at - AUDITION_AROUND).max(0.0);
    let to = (at + AUDITION_AROUND).min(duration);
    TimelineMix::of(clips).render(None, from, to, cancelled)
}
