// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Local hard-cut analysis using decoded frames and their presentation times.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use concat_core::frame::Frame;
use concat_media::{ColorRange, DecodeOptions, Decoder, FrameSource};
use concat_project::model::ranges::MIN_CLIP_DURATION;

/// A range between two detected cuts in a video.
pub struct Scene {
    /// The presentation time of the first frame, relative to the source.
    pub start: f64,
    /// The start of the next scene or the end of the video.
    pub end: f64,
    /// The first decoded frame, scaled for the browser.
    pub thumbnail: Frame,
}

/// Why the scan did not produce a complete scene list.
#[derive(Debug)]
pub enum ScanError {
    /// The browser closed or changed sources during analysis.
    Cancelled,
    /// The path does not name a local file.
    Missing,
    /// The video stream returned no pictures.
    NoFrames,
    /// The source is shorter than the timeline's minimum clip duration.
    TooShort,
    /// The source could not be probed or decoded.
    Decode(concat_media::Error),
}

/// Finds hard cuts in a video without pacing or seeking over source frames.
/// Both endpoints are in seconds from the start of the media.
pub fn analyse(
    path: &Path,
    media_duration: Option<f64>,
    range: Option<ColorRange>,
    cancelled: &AtomicBool,
) -> Result<Vec<Scene>, ScanError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    if !path.is_file() {
        return Err(ScanError::Missing);
    }
    let info = concat_media::probe(path).map_err(ScanError::Decode)?;
    let video = info.require_video().map_err(ScanError::Decode)?;
    let scale = (160.0 / f64::from(video.width))
        .min(90.0 / f64::from(video.height))
        .min(1.0);
    let even = |pixels| ((f64::from(pixels) * scale).round() as u32).max(2) & !1;
    let options = DecodeOptions::default()
        .scaled_to(even(video.width), even(video.height))
        .threaded(2)
        .in_range(range);
    let mut source = Decoder::open(path, &options).map_err(ScanError::Decode)?;
    let duration = [media_duration, info.duration.map(|time| time.as_f64())]
        .into_iter()
        .flatten()
        .filter(|length| length.is_finite() && *length > 0.0)
        .reduce(f64::min);
    scan(
        &mut source,
        duration,
        video.frame_rate.frame_duration().as_f64(),
        cancelled,
    )
}

fn scan(
    source: &mut impl FrameSource,
    duration: Option<f64>,
    frame_duration: f64,
    cancelled: &AtomicBool,
) -> Result<Vec<Scene>, ScanError> {
    let mut boundaries: Vec<(f64, Frame)> = Vec::new();
    let mut previous: Option<Frame> = None;
    let mut pending: Option<(f64, Frame)> = None;
    let mut last_stamp: Option<f64> = None;
    let mut inferred_end = 0.0_f64;
    let mut motion = 0.0_f64;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        let Some(frame) = source.next_frame().map_err(ScanError::Decode)? else {
            break;
        };
        let stamp = source
            .position()
            .map(|time| time.as_f64())
            .filter(|time| time.is_finite());
        let advancing_stamp = stamp.filter(|time| last_stamp.is_none_or(|last| *time > last));
        if let Some(time) = advancing_stamp {
            last_stamp = Some(time);
        }
        inferred_end = if let Some(time) = advancing_stamp {
            inferred_end.max(time + frame_duration)
        } else {
            inferred_end + frame_duration
        };

        let Some(before) = previous.replace(frame) else {
            boundaries.push((0.0, previous.as_ref().unwrap().clone()));
            continue;
        };
        let current = previous.as_ref().unwrap();
        let delta = difference(&before, current);
        let flash = if let Some((time, prior)) = pending.take() {
            // Hold the decision for one picture. A bright flash and its
            // immediate return must not register as two edits.
            let returned = difference(&prior, current) < 0.07;
            if !returned {
                boundaries.push((time, before.clone()));
            }
            returned
        } else {
            false
        };

        let scene_start = boundaries.last().unwrap().0;
        let cut_at = advancing_stamp.filter(|time| {
            *time - scene_start >= MIN_CLIP_DURATION
                && duration.is_none_or(|end| end - *time >= MIN_CLIP_DURATION)
        });
        // Compare an abrupt change with the amount of motion already in the shot.
        if !flash && delta >= (3.0 * motion).max(0.16) {
            if let Some(time) = cut_at {
                pending = Some((time, before));
            }
        } else if !flash {
            motion = 0.9 * motion + 0.1 * delta;
        }
    }

    let Some(last) = previous else {
        return Err(ScanError::NoFrames);
    };
    let end = duration.unwrap_or(inferred_end);
    if end < MIN_CLIP_DURATION {
        return Err(ScanError::TooShort);
    }
    if let Some((time, _)) = pending
        && end - time >= MIN_CLIP_DURATION
    {
        boundaries.push((time, last));
    }

    let ends: Vec<_> = boundaries
        .iter()
        .skip(1)
        .map(|(start, _)| *start)
        .chain([end])
        .collect();
    Ok(boundaries
        .into_iter()
        .zip(ends)
        .map(|((start, thumbnail), end)| Scene {
            start,
            end,
            thumbnail,
        })
        .collect())
}

fn difference(a: &Frame, b: &Frame) -> f64 {
    let mut distance = 0_u32;
    let mut count = 0_u32;
    for (a, b) in a
        .pixels()
        .chunks_exact(4)
        .zip(b.pixels().chunks_exact(4))
        .step_by(3)
    {
        distance += u32::from(a[0].abs_diff(b[0]))
            + u32::from(a[1].abs_diff(b[1]))
            + u32::from(a[2].abs_diff(b[2]));
        count += 1;
    }
    f64::from(distance) / (f64::from(count.max(1)) * 765.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_core::time::Rational;

    struct Frames {
        frames: std::vec::IntoIter<(i64, [u8; 4])>,
        at: Option<Rational>,
    }

    impl Frames {
        fn new(frames: Vec<(i64, [u8; 4])>) -> Self {
            Self {
                frames: frames.into_iter(),
                at: None,
            }
        }
    }

    impl FrameSource for Frames {
        fn width(&self) -> u32 {
            4
        }

        fn height(&self) -> u32 {
            4
        }

        fn next_frame(&mut self) -> concat_media::Result<Option<Frame>> {
            Ok(self.frames.next().map(|(milliseconds, pixel)| {
                self.at = Some(Rational::new(milliseconds, 1000));
                Frame::from_rgba(self.width(), self.height(), pixel.repeat(16)).unwrap()
            }))
        }

        fn position(&self) -> Option<Rational> {
            self.at
        }
    }

    const RED: [u8; 4] = [240, 5, 5, 255];
    const BLUE: [u8; 4] = [5, 5, 240, 255];

    #[test]
    fn scene_boundary_uses_variable_rate_frame_stamp_and_its_picture() {
        let mut source = Frames::new(vec![
            (0, RED),
            (40, RED),
            (271, BLUE),
            (309, BLUE),
            (630, BLUE),
        ]);
        let scenes = scan(&mut source, Some(0.8), 0.04, &AtomicBool::new(false)).unwrap();
        assert_eq!(scenes.len(), 2);
        assert_eq!((scenes[0].start, scenes[0].end), (0.0, 0.271));
        assert_eq!((scenes[1].start, scenes[1].end), (0.271, 0.8));
        assert_eq!(&scenes[1].thumbnail.pixels()[..4], &BLUE);
    }

    #[test]
    fn an_isolated_flash_stays_in_one_scene() {
        let mut source = Frames::new(vec![
            (0, RED),
            (40, RED),
            (80, [255; 4]),
            (120, RED),
            (160, RED),
        ]);
        let scenes = scan(&mut source, None, 0.04, &AtomicBool::new(false)).unwrap();
        assert_eq!(scenes.len(), 1);
        assert!((scenes[0].end - 0.2).abs() < 1e-9);
    }

    #[test]
    fn cancelled_analysis_does_not_read_the_source() {
        let mut source = Frames::new(vec![(0, RED)]);
        assert!(matches!(
            scan(&mut source, Some(0.04), 0.04, &AtomicBool::new(true)),
            Err(ScanError::Cancelled)
        ));
        assert!(source.position().is_none());
    }
}
