// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

/// Fit a single scale into an HD frame in the input's orientation.
pub(super) fn dimensions(width: u32, height: u32) -> Option<(u32, u32)> {
    let (bound_w, bound_h) = if width >= height {
        (1280, 720)
    } else {
        (720, 1280)
    };
    let scale = (f64::from(bound_w) / f64::from(width))
        .min(f64::from(bound_h) / f64::from(height))
        .min(1.0);
    let round_even = |original: u32, bound| {
        let scaled = ((f64::from(original) * scale / 2.0).round() as u32) * 2;
        scaled.min(original & !1).min(bound)
    };
    let w = round_even(width, bound_w);
    let h = round_even(height, bound_h);
    (w >= 2 && h >= 2).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_landscape_portrait_and_square_without_stretching() {
        assert_eq!(dimensions(3840, 2160), Some((1280, 720)));
        assert_eq!(dimensions(2160, 3840), Some((720, 1280)));
        assert_eq!(dimensions(1600, 1600), Some((720, 720)));
        assert_eq!(dimensions(1920, 1440), Some((960, 720)));
        assert_eq!(dimensions(2560, 1080), Some((1280, 540)));
    }

    #[test]
    fn keeps_a_small_project_small_and_writes_even_dimensions() {
        assert_eq!(dimensions(640, 480), Some((640, 480)));
        assert_eq!(dimensions(639, 479), Some((638, 478)));
        assert_eq!(dimensions(0, 480), None);
    }
}
