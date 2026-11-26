// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Follows an object on rendered frames, on a worker owned by the editor.

use std::sync::atomic::{AtomicBool, Ordering};

use concat_core::frame::Frame;
use concat_core::time::FrameRate;
use concat_export::{ExportClip, PreviewPlan};
use concat_media::ReaderPool;
use concat_project::model::VideoSettings;
use concat_vision::object_follow::ObjectTracker;
use concat_vision::stabilize::ANALYSIS_SIDE;
use concat_vision::stabilize::track::Gray;

pub use concat_vision::object_follow::{ObjectOffset, ObjectRegion};

const FRAME_CACHE_BYTES: usize = 32 * 1024 * 1024;
const KEY_TOLERANCE_PIXELS: f64 = 1.5;

/// A snapshot of the single video clip and the fragment selected for tracking.
pub struct FollowRequest {
    /// The target clip with its timeline transforms and speed already resolved.
    pub clip: ExportClip,
    /// The output frame to track, including its dimensions and cadence.
    pub video: VideoSettings,
    /// The object drawn around on the first rendered frame.
    pub region: ObjectRegion,
    /// The first instant of the overlapping video and graphic.
    pub start: f64,
    /// The exclusive end of the overlap, in timeline seconds.
    pub end: f64,
}

/// The displacement captured at one point on the track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FollowSample {
    /// Timeline seconds; the caller converts that to the graphic's clip keys.
    pub time: f64,
    /// Fractions of the output frame, relative to the selection's position.
    pub offset: ObjectOffset,
}

/// The tracking path up to the end of the fragment or the object's loss.
pub struct FollowResult {
    /// An ordered path, simplified to keep the keyframe editor responsive.
    pub samples: Vec<FollowSample>,
    /// The first frame where the object could no longer be followed.
    pub lost_at: Option<f64>,
}

/// A tracking attempt that could not produce a usable path.
#[derive(Debug)]
pub enum FollowError {
    /// The owner cancelled the operation.
    Cancelled,
    /// There are fewer than two frames in the chosen interval.
    NoInterval,
    /// The first frame has too little local detail to seed the tracker.
    NoFeatures,
    /// The decoder or compositor could not supply an exact frame.
    ReadFrame(String),
}

/// Analyses rendered output coordinates without touching the document or the
/// live monitor. The caller owns cancellation and decides whether to apply keys.
pub fn analyse(
    request: &FollowRequest,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> Result<FollowResult, FollowError> {
    let video = request.video;
    let rate = FrameRate::checked(video.rate_num, video.rate_den)
        .map(|rate| rate.fps().as_f64())
        .ok_or(FollowError::NoInterval)?;
    if !request.start.is_finite()
        || !request.end.is_finite()
        || request.start < 0.0
        || request.end <= request.start
    {
        return Err(FollowError::NoInterval);
    }
    let first = (request.start * rate + 1e-6).floor() as i64;
    let end = (request.end * rate - 1e-6).ceil() as i64;
    if end.saturating_sub(first) < 2 {
        return Err(FollowError::NoInterval);
    }

    let scale = f64::from(ANALYSIS_SIDE) / f64::from(video.width.max(video.height).max(1));
    let side = |pixels: u32| ((f64::from(pixels) * scale.min(1.0)).round() as u32).max(2) & !1;
    let (width, height) = (side(video.width), side(video.height));
    let plan = concat_export::preview_plan(
        std::slice::from_ref(&request.clip),
        width,
        height,
        video.rate_num,
        video.rate_den,
        video.color_space,
        video.canvas,
    );
    let pool = ReaderPool::new(FRAME_CACHE_BYTES, 2);
    check_cancelled(cancelled)?;
    let anchor = read_frame(&pool, &plan, request.start, width, height)?;
    let mut tracker =
        ObjectTracker::start(anchor, request.region).ok_or(FollowError::NoFeatures)?;
    let mut samples = vec![FollowSample {
        time: request.start,
        offset: ObjectOffset::default(),
    }];
    let mut lost_at = None;
    let mut reported = 0;
    for frame in first + 1..end {
        check_cancelled(cancelled)?;
        let at = frame as f64 / rate;
        let picture = read_frame(&pool, &plan, at, width, height)?;
        let Some(offset) = tracker.push(picture) else {
            lost_at = Some(at);
            break;
        };
        samples.push(FollowSample { time: at, offset });
        let percent = ((frame - first) as f64 * 100.0 / (end - first) as f64) as u32;
        if percent != reported {
            progress(percent as f32 / 100.0);
            reported = percent;
        }
    }
    check_cancelled(cancelled)?;
    let samples = simplify(samples, f64::from(video.width), f64::from(video.height));
    progress(1.0);
    Ok(FollowResult { samples, lost_at })
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), FollowError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(FollowError::Cancelled);
    }
    Ok(())
}

fn read_frame(
    pool: &ReaderPool,
    plan: &PreviewPlan,
    time: f64,
    width: u32,
    height: u32,
) -> Result<Gray, FollowError> {
    let pixels = concat_export::preview_sources_of(pool, plan, time, false, false)
        .and_then(|sources| sources.pixels(true))
        .map_err(FollowError::ReadFrame)?;
    let frame = Frame::from_rgba(width, height, pixels)
        .ok_or_else(|| FollowError::ReadFrame("unexpected tracking frame size".into()))?;
    Ok(Gray::of_frame(&frame))
}

fn simplify(samples: Vec<FollowSample>, width: f64, height: f64) -> Vec<FollowSample> {
    if samples.len() <= 2 {
        return samples;
    }

    let last = samples.len() - 1;
    let mut kept = vec![false; samples.len()];
    kept[0] = true;
    kept[last] = true;
    let mut pending = vec![(0, last)];
    while let Some((first, last)) = pending.pop() {
        let a = samples[first];
        let b = samples[last];
        let span = b.time - a.time;
        let mut farthest = None;
        let mut error = KEY_TOLERANCE_PIXELS;
        for (index, point) in samples.iter().enumerate().take(last).skip(first + 1) {
            let fraction = (point.time - a.time) / span;
            let x = a.offset.x + (b.offset.x - a.offset.x) * fraction;
            let y = a.offset.y + (b.offset.y - a.offset.y) * fraction;
            let distance = ((point.offset.x - x) * width).hypot((point.offset.y - y) * height);
            if distance > error {
                error = distance;
                farthest = Some(index);
            }
        }
        if let Some(index) = farthest {
            kept[index] = true;
            pending.extend([(first, index), (index, last)]);
        }
    }
    samples
        .into_iter()
        .zip(kept)
        .filter_map(|(sample, keep)| keep.then_some(sample))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_redundant_keys_but_keeps_changes_in_the_objects_direction() {
        let samples = (0..=8)
            .map(|index| FollowSample {
                time: f64::from(index),
                offset: ObjectOffset {
                    x: f64::from(index.min(8 - index)) * 0.02,
                    y: 0.0,
                },
            })
            .collect();
        let retained = simplify(samples, 1920.0, 1080.0);
        assert_eq!(
            retained.iter().map(|point| point.time).collect::<Vec<_>>(),
            [0.0, 4.0, 8.0]
        );
    }
}
