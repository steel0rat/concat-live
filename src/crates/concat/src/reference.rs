// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

use std::path::Path;
use std::sync::Arc;

use concat_core::time::FrameRate;
use concat_export::{ClipKind, ExportClip};
use concat_host::preview::{FrameSpec, Monitor};
use concat_project::DocumentSettings;
use concat_project::model::{ColorSpace, MediaKind, OPAQUE_BLACK};
use slint::ComponentHandle;

use crate::host::{self, Shell};
use crate::i18n::t;
use crate::panes::monitor::Picture;
use crate::studio::Studio;
use crate::ui::{App, DockSide, PaneKind, ReferencePreview};

const MAX_PREVIEW_SIDE: u32 = 1280;
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "m4v", "mpeg", "mpg", "mxf", "mts",
];

#[derive(Default)]
pub(crate) struct ReferenceState {
    file: Option<ReferenceFile>,
    monitor: Option<Monitor>,
    frame: slint::Image,
    loading: bool,
    error: Option<ReferenceError>,
    generation: u64,
    completed: Option<FrameKey>,
    requested: Option<FrameKey>,
    target: Option<FrameKey>,
    decode_failed: bool,
}

struct ReferenceFile {
    settings: DocumentSettings,
    clips: Arc<Vec<ExportClip>>,
    duration: f64,
    rate: f64,
    size: (u32, u32),
}

#[derive(Clone, Copy)]
enum ReferenceError {
    Open,
    VideoRequired,
    DurationUnavailable,
    Preview,
}

impl ReferenceError {
    fn text(self) -> String {
        match self {
            Self::Open => t("reference.openFailed"),
            Self::VideoRequired => t("reference.videoRequired"),
            Self::DurationUnavailable => t("reference.durationUnavailable"),
            Self::Preview => t("reference.previewFailed"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FrameKey {
    index: i64,
    color: ColorSpace,
}

impl ReferenceFile {
    fn from_path(path: &Path) -> Result<Self, ReferenceError> {
        let path = path
            .to_str()
            .filter(|_| path.is_absolute() && path.is_file())
            .ok_or(ReferenceError::Open)?;
        let summary = concat_host::media::probe(path).map_err(|error| {
            log::warn!("reference: probing video failed: {error}");
            ReferenceError::Open
        })?;
        let Some(video) = summary
            .video
            .as_ref()
            .filter(|_| summary.kind == MediaKind::Video)
        else {
            return Err(ReferenceError::VideoRequired);
        };
        let duration = summary
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .ok_or(ReferenceError::DurationUnavailable)?;
        let (n, d) = video
            .frame_rate_fraction
            .split_once('/')
            .ok_or(ReferenceError::Open)?;
        let (num, den) = n
            .parse()
            .ok()
            .zip(d.parse().ok())
            .ok_or(ReferenceError::Open)?;
        let rate = FrameRate::checked(num, den).ok_or(ReferenceError::Open)?;
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_owned();
        let settings = DocumentSettings {
            name,
            width: video.width,
            height: video.height,
            rate_num: rate.fps().numerator(),
            rate_den: rate.fps().denominator(),
            canvas: OPAQUE_BLACK,
        };
        let clip = ExportClip {
            path: summary.path,
            media_width: Some(video.width),
            media_height: Some(video.height),
            media_duration: Some(duration),
            has_audio: Some(false),
            muted: true,
            volume: 0.0,
            ..ExportClip::blank(ClipKind::Video, 0.0, duration, 0)
        };
        Ok(Self {
            settings,
            clips: Arc::new(vec![clip]),
            duration,
            rate: rate.fps().as_f64(),
            size: preview_size(video.width, video.height),
        })
    }
}

fn frame_index(time: f64, duration: f64, rate: f64) -> Option<i64> {
    (time >= 0.0 && time < duration).then(|| ((time * rate) + 1e-6).floor() as i64)
}

fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let factor = (f64::from(MAX_PREVIEW_SIDE) / f64::from(width.max(height).max(1))).min(1.0);
    let side = |value: u32| ((f64::from(value) * factor).round() as u32).max(2) & !1;
    (side(width), side(height))
}

pub(crate) fn choose(seat: i32) {
    Shell::with(|shell, _| {
        if shell.studio.borrow().session.is_none() {
            return;
        }
        let epoch = host::project_epoch();
        host::pick_files_then(
            t("reference.choose"),
            Some((t("reference.videoFiles"), VIDEO_EXTENSIONS)),
            move |studio, app, _, paths| {
                if epoch != host::project_epoch() || studio.session.is_none() {
                    return;
                }
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let seat = seat.max(0) as usize;
                if !studio.dock.contains(PaneKind::Reference) {
                    let target = if studio.dock.kind_at(seat) == Some(PaneKind::Preview) {
                        Some(seat)
                    } else {
                        studio
                            .dock_layout()
                            .seats
                            .iter()
                            .find(|pane| pane.kind == PaneKind::Preview)
                            .map(|pane| pane.index as usize)
                    };
                    let Some(seat) = target else { return };
                    if let Some(path) = studio.dock.leaf_path(seat) {
                        studio
                            .dock
                            .split_leaf(&path, PaneKind::Reference, DockSide::Right);
                        studio.remember_layout_soon();
                    }
                }
                load(studio, path);
                publish(studio, app);
            },
        );
    });
}

fn load(studio: &mut Studio, path: std::path::PathBuf) {
    let reference = &mut studio.reference;
    reference.generation += 1;
    let generation = reference.generation;
    reference.loading = true;
    reference.error = None;
    reference.requested = None;
    host::spawn_in_project_publishing(
        move || ReferenceFile::from_path(&path),
        move |studio, _, _, result| {
            let reference = &mut studio.reference;
            if generation != reference.generation {
                return;
            }
            reference.loading = false;
            match result {
                Ok(file) => {
                    reference
                        .monitor
                        .get_or_insert_with(|| studio.host.monitor.fork());
                    reference.file = Some(file);
                    reference.completed = None;
                    reference.target = None;
                    reference.frame = slint::Image::default();
                    reference.decode_failed = false;
                    studio.close_source();
                }
                Err(error) => reference.error = Some(error),
            }
            request(studio);
        },
        |studio, app, _| publish(studio, app),
    );
}

pub(crate) fn clear(studio: &mut Studio) {
    let reference = &mut studio.reference;
    *reference = ReferenceState {
        generation: reference.generation + 1,
        ..ReferenceState::default()
    };
}

pub(crate) fn seek(studio: &mut Studio, seconds: f32) {
    if let Some(file) = studio.reference.file.as_ref()
        && studio.source.is_none()
        && seconds.is_finite()
    {
        studio.seek(seconds.clamp(0.0, file.duration as f32));
    }
}

pub(crate) fn toggle_playback(studio: &mut Studio) {
    if studio.reference.file.is_some() && studio.source.is_none() {
        studio.play_toggle();
    }
}

pub(crate) fn request(studio: &mut Studio) {
    let visible = !studio.on_start && studio.dock.contains(PaneKind::Reference);
    let time = studio.preview_time();
    let source_preview = studio.source.is_some();
    let color = studio.timeline().video.color_space;
    let reference = &mut studio.reference;
    let Some(file) = reference.file.as_ref() else {
        return;
    };
    reference.target = frame_index(time, file.duration, file.rate)
        .filter(|_| visible && !source_preview)
        .map(|index| FrameKey { index, color });
    let Some(key) = reference.target else { return };
    if reference.loading
        || reference.decode_failed
        || reference.requested.is_some()
        || reference.completed == Some(key)
    {
        return;
    }
    let Some(monitor) = reference.monitor.clone() else {
        return;
    };
    reference.requested = Some(key);
    let generation = reference.generation;
    let clips = Arc::clone(&file.clips);
    let settings = file.settings.clone();
    let (width, height) = file.size;
    let spec = FrameSpec {
        time: key.index as f64 / file.rate,
        width,
        height,
        moving: studio.playing,
        proxy: false,
        color_space: key.color,
        canvas: OPAQUE_BLACK,
        nearest: false,
    };
    host::spawn_in_project_publishing(
        move || {
            if monitor.has_gpu() {
                monitor
                    .frame_sources(clips, &settings, spec)
                    .map(Picture::Sources)
            } else {
                monitor
                    .frame_pixels(clips, &settings, spec)
                    .map(|(pixels, exact)| Picture::Pixels(pixels, width, height, exact))
            }
        },
        move |studio, _, _, result| {
            let reference = &mut studio.reference;
            if generation != reference.generation || reference.requested != Some(key) {
                return;
            }
            reference.requested = None;
            let show = reference.target.is_some_and(|wanted| {
                wanted == key || (studio.playing && wanted.color == key.color)
            });
            if show {
                match image(reference.monitor.as_ref(), result, spec) {
                    Ok(frame) => {
                        reference.frame = frame;
                        reference.completed = Some(key);
                    }
                    Err(error) => {
                        log::warn!("reference: preview failed: {error}");
                        reference.error = Some(ReferenceError::Preview);
                        reference.decode_failed = true;
                    }
                }
            }
            request(studio);
        },
        |studio, app, _| publish(studio, app),
    );
}

fn image(
    monitor: Option<&Monitor>,
    result: Result<Picture, String>,
    spec: FrameSpec,
) -> Result<slint::Image, String> {
    match result? {
        Picture::Sources(sources) => monitor
            .ok_or_else(|| "missing reference monitor".to_owned())?
            .texture_of(&sources, spec)
            .and_then(|texture| slint::Image::try_from(texture).map_err(|error| error.to_string())),
        Picture::Pixels(bytes, width, height, _) => {
            Ok(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                slint::Rgba8Pixel,
            >::clone_from_slice(
                &bytes, width, height
            )))
        }
    }
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let panel = app.global::<ReferencePreview>();
    let reference = &studio.reference;
    panel.set_has_file(reference.file.is_some());
    panel.set_loading(reference.loading);
    panel.set_error(
        reference
            .error
            .map(ReferenceError::text)
            .unwrap_or_default()
            .into(),
    );
    panel.set_frame(reference.frame.clone());
    panel.set_frame_ready(reference.completed.is_some() && !reference.decode_failed);
    if let Some(file) = reference.file.as_ref() {
        panel.set_file_name(file.settings.name.as_str().into());
        panel.set_duration(file.duration as f32);
        panel.set_rate(file.rate as f32);
    }
    publish_position(studio, app);
}

pub(crate) fn publish_position(studio: &Studio, app: &App) {
    let Some(file) = studio.reference.file.as_ref() else {
        return;
    };
    let panel = app.global::<ReferencePreview>();
    let showing_source = studio.source.is_some();
    let time = if showing_source {
        f64::from(studio.playhead)
    } else {
        studio.preview_time()
    };
    panel.set_time(time as f32);
    panel.set_past_end(time >= file.duration);
    panel.set_playing(studio.playing);
    panel.set_showing_source(showing_source);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_seconds_map_to_reference_frames_at_its_actual_rate() {
        assert_eq!(frame_index(2.5, 10.0, 24.0), Some(60));
        assert_eq!(frame_index(2.5, 10.0, 30_000.0 / 1_001.0), Some(74));
        assert_eq!(frame_index(9.999, 10.0, 24.0), Some(239));
        assert_eq!(frame_index(10.0, 10.0, 24.0), None);
        assert_eq!(frame_index(f64::NAN, 10.0, 24.0), None);
    }

    #[test]
    fn reference_preview_keeps_the_native_aspect_and_bounds_the_texture() {
        assert_eq!(preview_size(3840, 2160), (1280, 720));
        assert_eq!(preview_size(2160, 3840), (720, 1280));
        assert_eq!(preview_size(320, 180), (320, 180));
    }
}
