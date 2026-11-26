// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Unmastered, timeline-aligned audio stems from the existing export mixer.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use concat_core::time::FrameRate;
use concat_media::audio::{self, AudioClip};
use concat_project::Project;

use super::export_folder::OutputSet;
use super::{ClipKind, ExportClip, audio_pieces, flatten, resolve_transitions};

/// The export group assigned to a timeline track.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StemRole {
    /// Dialogue, narration or another voice.
    Voice,
    /// A score or backing music.
    Music,
    /// Sound effects and ambience.
    Effects,
}

impl StemRole {
    fn name(self) -> &'static str {
        match self {
            Self::Voice => "voice.wav",
            Self::Music => "music.wav",
            Self::Effects => "effects.wav",
        }
    }
}

/// Why export did not create a complete folder of stems.
#[derive(Debug)]
pub enum StemError {
    /// No track with sound is assigned to a role.
    Empty,
    /// The active cut has an unsupported duration or rate.
    Timing,
    /// The export folder or requested name is invalid.
    Destination,
    /// The export was stopped before the complete set was published.
    Cancelled,
    /// Writing or moving the new files failed.
    Files(std::io::Error),
    /// The native audio mixer could not render a stem.
    Mix {
        /// The affected role.
        role: StemRole,
        /// The audio engine error.
        error: concat_media::Error,
    },
}

const MAX_WAVE_BYTES: f64 = u32::MAX as f64 - 1024.0;

/// Exports each assigned role into a fresh folder. Every WAV starts at zero
/// and has the same length, with clip treatments and transitions applied
/// before the treatment of the master mix.
pub fn export(
    project: &Project,
    project_dir: &Path,
    roles: &BTreeMap<String, StemRole>,
    directory: &Path,
    name: &str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, usize, StemRole),
) -> Result<PathBuf, StemError> {
    let single_name = matches!(
        Path::new(name).components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    );
    if !directory.is_dir() || !single_name {
        return Err(StemError::Destination);
    }
    let timeline = project.active();
    let frame_rate = FrameRate::checked(timeline.video.rate_num, timeline.video.rate_den)
        .ok_or(StemError::Timing)?;
    let seconds = timeline.duration();
    if !seconds.is_finite()
        || seconds <= 0.0
        || seconds * f64::from(audio::MIX_RATE) * 4.0 > MAX_WAVE_BYTES
    {
        return Err(StemError::Timing);
    }

    let mut flattened = flatten::flatten_timeline_in(project, None, Some(project_dir));
    for clip in &mut flattened {
        if !clip.path.is_empty() && Path::new(&clip.path).is_relative() {
            clip.path = project_dir.join(&clip.path).to_string_lossy().into_owned();
        }
    }
    resolve_transitions(&mut flattened, frame_rate);
    let assigned = timeline
        .tracks
        .iter()
        .map(|track| roles.get(&track.id).copied())
        .collect::<Vec<_>>();
    let mut groups: BTreeMap<StemRole, Vec<AudioClip>> = BTreeMap::new();
    for clip in flattened.iter().filter(|clip| is_audible(clip)) {
        let Some(role) = assigned.get(clip.track).copied().flatten() else {
            continue;
        };
        let pieces = audio_pieces(clip);
        if !pieces.is_empty() {
            groups.entry(role).or_default().extend(pieces);
        }
    }
    if groups.is_empty() {
        return Err(StemError::Empty);
    }

    let stopped = || cancel.load(Ordering::Relaxed);
    if stopped() {
        return Err(StemError::Cancelled);
    }
    let members = groups.keys().map(|role| role.name()).collect::<Vec<_>>();
    let staged = OutputSet::new(directory, &members).map_err(StemError::Files)?;
    let total = groups.len();
    for (index, (role, clips)) in groups.into_iter().enumerate() {
        if stopped() {
            return Err(StemError::Cancelled);
        }
        progress(index + 1, total, role);
        audio::mix_to_file(&clips, seconds, &staged.path().join(role.name()))
            .map_err(|error| StemError::Mix { role, error })?;
    }
    if stopped() {
        return Err(StemError::Cancelled);
    }
    staged.publish(name).map_err(StemError::Files)
}

fn is_audible(clip: &ExportClip) -> bool {
    matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
        && !clip.muted
        && clip.has_audio.unwrap_or_else(|| {
            concat_media::probe(&clip.path).is_ok_and(|probe| probe.audio.is_some())
        })
}
