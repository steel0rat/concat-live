// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Tracks the selected patch, returning its displacement in output coordinates.

use crate::stabilize::fit::{Pair, ransac};
use crate::stabilize::track::{Gray, Pyramid};

const MIN_TRACKS: usize = 4;
const MAX_TRACKS: usize = 120;
const ROUND_TRIP_LIMIT: f32 = 0.7;

/// The rectangle where the first frame's tracking features are chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectRegion {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl ObjectRegion {
    /// Both corners of the region, as fractions of the rendered frame.
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Option<Self> {
        if [x1, y1, x2, y2].iter().any(|value| !value.is_finite()) {
            return None;
        }
        let region = Self {
            left: x1.min(x2).clamp(0.0, 1.0),
            top: y1.min(y2).clamp(0.0, 1.0),
            right: x1.max(x2).clamp(0.0, 1.0),
            bottom: y1.max(y2).clamp(0.0, 1.0),
        };
        (region.right > region.left && region.bottom > region.top).then_some(region)
    }
}

/// The object's translation since its first frame, in fractions of frame width
/// and height respectively. Positive y moves down.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ObjectOffset {
    /// Horizontal displacement divided by output width.
    pub x: f64,
    /// Vertical displacement divided by output height.
    pub y: f64,
}

/// An optical tracker whose support is drawn only from the chosen object. It
/// leaves the run stopped when the original features can no longer be followed.
pub struct ObjectTracker {
    previous: Option<Pyramid>,
    points: Vec<(f32, f32)>,
    origin: (f64, f64),
    center: (f64, f64),
}

impl ObjectTracker {
    /// Seeds the tracker from the selected rectangle on the first rendered frame.
    pub fn start(frame: Gray, region: ObjectRegion) -> Option<Self> {
        if frame.width.checked_mul(frame.height)? != frame.data.len() {
            return None;
        }
        let (width, height) = (frame.width as f64, frame.height as f64);
        let (left, top) = (
            (region.left * width).floor() as usize,
            (region.top * height).floor() as usize,
        );
        let (right, bottom) = (
            (region.right * width).ceil() as usize,
            (region.bottom * height).ceil() as usize,
        );
        if right <= left || bottom <= top {
            return None;
        }

        // Corner detection uses the contrast of this patch. A high-contrast
        // background outside it cannot set the threshold or supply candidates.
        let patch = Gray {
            width: right - left,
            height: bottom - top,
            data: (top..bottom)
                .flat_map(|y| {
                    frame.data[y * frame.width + left..y * frame.width + right]
                        .iter()
                        .copied()
                })
                .collect(),
        };
        let initial = Pyramid::of(patch).corners(20, 4, 5.0, &[]);
        if initial.len() < MIN_TRACKS {
            return None;
        }
        let stride = initial.len().div_ceil(MAX_TRACKS);
        let points = initial
            .into_iter()
            .step_by(stride)
            .map(|(x, y)| (x + left as f32, y + top as f32))
            .collect();
        let center = (
            (region.left + region.right) * width / 2.0 - 0.5,
            (region.top + region.bottom) * height / 2.0 - 0.5,
        );
        Some(Self {
            previous: Some(Pyramid::of(frame)),
            points,
            origin: center,
            center,
        })
    }

    /// Advances one frame. None means tracking was lost and cannot continue
    /// with this selection.
    pub fn push(&mut self, frame: Gray) -> Option<ObjectOffset> {
        let previous = self.previous.take()?;
        let (width, height) = (frame.width, frame.height);
        if (previous.base().width, previous.base().height) != (width, height)
            || previous.base().data.len() != frame.data.len()
        {
            return None;
        }
        let next = Pyramid::of(frame);
        let pairs: Vec<Pair> = self
            .points
            .iter()
            .copied()
            .filter_map(|from| {
                let to = previous.flow(&next, from)?;
                let back = next.flow(&previous, to)?;
                let round_trip = (back.0 - from.0).hypot(back.1 - from.1);
                (round_trip <= ROUND_TRIP_LIMIT
                    && similar_patch(previous.base(), next.base(), from, to))
                .then_some((
                    (f64::from(from.0), f64::from(from.1)),
                    (f64::from(to.0), f64::from(to.1)),
                ))
            })
            .collect();
        let min_inliers = MIN_TRACKS.max(self.points.len().div_ceil(3));
        let fit = ransac(&pairs, 1.5, min_inliers)?;
        let center = fit.similarity.apply(self.center);
        let (width, height) = (width as f64, height as f64);
        if !(0.0..width).contains(&center.0) || !(0.0..height).contains(&center.1) {
            return None;
        }

        self.points = pairs
            .into_iter()
            .zip(fit.inliers)
            .filter_map(|((_, (x, y)), agreed)| agreed.then_some((x as f32, y as f32)))
            .collect();
        self.previous = Some(next);
        self.center = center;
        Some(ObjectOffset {
            x: (center.0 - self.origin.0) / width,
            y: (center.1 - self.origin.1) / height,
        })
    }
}

fn similar_patch(previous: &Gray, next: &Gray, from: (f32, f32), to: (f32, f32)) -> bool {
    let mut values = [(0.0_f32, 0.0_f32); 25];
    let mut from_sum = 0.0;
    let mut to_sum = 0.0;
    for (index, sample) in values.iter_mut().enumerate() {
        let x = (index % 5) as f32 * 2.0 - 4.0;
        let y = (index / 5) as f32 * 2.0 - 4.0;
        let a = previous.sample(from.0 + x, from.1 + y);
        let b = next.sample(to.0 + x, to.1 + y);
        *sample = (a, b);
        from_sum += a;
        to_sum += b;
    }
    let from_mean = from_sum / values.len() as f32;
    let to_mean = to_sum / values.len() as f32;
    let (mut aa, mut bb, mut ab) = (0.0, 0.0, 0.0);
    for (a, b) in values {
        let a = a - from_mean;
        let b = b - to_mean;
        aa += a * a;
        bb += b * b;
        ab += a * b;
    }
    aa > 0.0 && bb > 0.0 && ab >= 0.6 * (aa * bb).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: usize = 240;
    const HEIGHT: usize = 160;

    fn region() -> ObjectRegion {
        ObjectRegion::new(60.0 / 240.0, 42.0 / 160.0, 138.0 / 240.0, 116.0 / 160.0).unwrap()
    }

    fn scene(dx: usize, dy: usize, object: bool) -> Gray {
        let mut image = Gray::filled(WIDTH, HEIGHT, 0.0);
        let decoration =
            |x: usize, y: usize| ((x * 13 + y * 17 + (x / 7) * (y / 9) * 21) % 39) as f32;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                image.data[y * WIDTH + x] = 46.0 + decoration(x, y);
            }
        }
        if object {
            for y in 0..80 {
                for x in 0..84 {
                    let ring = ((x / 7 + y / 9) % 2) as f32;
                    image.data[(y + 39 + dy) * WIDTH + x + 57 + dx] =
                        70.0 + 115.0 * ring + decoration(x * 2, y * 3);
                }
            }
        }
        image
    }

    #[test]
    fn follows_the_moving_patch_with_a_stationary_background() {
        let mut tracker = ObjectTracker::start(scene(0, 0, true), region()).unwrap();
        for frame in 1..=8 {
            let offset = tracker
                .push(scene(frame * 3, frame * 2, true))
                .expect("object is still in frame");
            assert!(
                (offset.x * WIDTH as f64 - (frame * 3) as f64).abs() < 1.6,
                "{offset:?}"
            );
            assert!(
                (offset.y * HEIGHT as f64 - (frame * 2) as f64).abs() < 1.6,
                "{offset:?}"
            );
        }
    }

    #[test]
    fn loss_stops_tracking_instead_of_reattaching_to_background() {
        let mut tracker = ObjectTracker::start(scene(0, 0, true), region()).unwrap();
        assert!(tracker.push(scene(3, 2, true)).is_some());
        assert!(tracker.push(scene(0, 0, false)).is_none());
        assert!(tracker.push(scene(6, 4, true)).is_none());
    }

    #[test]
    fn a_flat_or_empty_selection_cannot_start_tracking() {
        assert!(ObjectTracker::start(Gray::filled(WIDTH, HEIGHT, 120.0), region()).is_none());
        assert!(ObjectRegion::new(0.4, 0.2, 0.4, 0.5).is_none());
    }
}
