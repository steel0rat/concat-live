// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use concat_text::{Align, Fonts, TitleStyle};
use image::{RgbaImage, imageops};

#[derive(Clone, Copy, Default)]
pub(crate) enum Aspect {
    #[default]
    Original,
    Wide,
    Square,
    Vertical,
}

impl Aspect {
    fn ratio(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Original => (width, height),
            Self::Wide => (16, 9),
            Self::Square => (1, 1),
            Self::Vertical => (9, 16),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Framing {
    pub(crate) aspect: Aspect,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) zoom: f32,
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            aspect: Aspect::Original,
            x: 0.5,
            y: 0.5,
            zoom: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Framing {
    fn crop(self, width: u32, height: u32) -> Option<Crop> {
        if width == 0
            || height == 0
            || ![self.x, self.y, self.zoom]
                .iter()
                .all(|value| value.is_finite())
        {
            return None;
        }
        let (a, b) = self.aspect.ratio(width, height);
        let [aspect_x, aspect_y] = [f64::from(a), f64::from(b)];
        let scale = (f64::from(width) / aspect_x).min(f64::from(height) / aspect_y)
            / f64::from(self.zoom.clamp(1.0, 4.0));
        let w = ((aspect_x * scale).round() as u32).clamp(1, width);
        let h = ((aspect_y * scale).round() as u32).clamp(1, height);
        let offset = |length, region, along: f32| {
            (f64::from(length - region) * f64::from(along.clamp(0.0, 1.0))).round() as u32
        };
        Some(Crop {
            x: offset(width, w, self.x),
            y: offset(height, h, self.y),
            width: w,
            height: h,
        })
    }
}

#[derive(Debug)]
pub(crate) enum CoverError {
    Frame,
    Title,
}

pub(crate) fn compose(
    original: &RgbaImage,
    framing: Framing,
    title: &str,
    fonts: &Fonts,
) -> Result<RgbaImage, CoverError> {
    let crop = framing
        .crop(original.width(), original.height())
        .ok_or(CoverError::Frame)?;
    let mut result =
        imageops::crop_imm(original, crop.x, crop.y, crop.width, crop.height).to_image();
    if title.trim().is_empty() {
        return Ok(result);
    }
    let height = crop.height;
    let band = height * 2 / 5;
    if band < 24 || crop.width < 48 {
        return Err(CoverError::Title);
    }

    let mut style = TitleStyle {
        content: title.trim().to_owned(),
        font_family: concat_text::BUNDLED_FAMILY.to_owned(),
        font_size: 0.22,
        font_weight: 800.0,
        italic: false,
        underline: false,
        strikethrough: false,
        color: "#ffffff".into(),
        align: Align::Center,
        stroke_width: 0.0,
        stroke_color: "#000000".into(),
        shadow: true,
        background: "#15171bd9".into(),
        background_radius: 0.026,
        background_padding_x: 0.045,
        background_padding_y: 0.025,
        line_height: 1.10,
        tracking: 0.0,
        max_width: 0.82,
        max_height: 0.0,
        runs: Vec::new(),
    };
    let width = result.width();
    for _ in 0..5 {
        let title =
            concat_text::render_frame(fonts, &style, width, band).map_err(|_| CoverError::Title)?;
        if title.block_height > band || title.block_width > width {
            style.font_size *= 0.78;
            continue;
        }
        let bitmap =
            RgbaImage::from_raw(title.width, title.height, title.rgba).ok_or(CoverError::Title)?;
        let x = (i64::from(width) - i64::from(title.width)) / 2;
        let y = i64::from(height)
            - i64::from(height / 20)
            - (i64::from(band) + i64::from(title.height)) / 2;
        imageops::overlay(&mut result, &bitmap, x, y);
        return Ok(result);
    }
    Err(CoverError::Title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_stays_inside_the_frame_and_can_slide_to_its_edges() {
        let portrait = Framing {
            aspect: Aspect::Vertical,
            x: 1.0,
            ..Framing::default()
        };
        assert_eq!(
            portrait.crop(1920, 1080),
            Some(Crop {
                x: 1312,
                y: 0,
                width: 608,
                height: 1080
            })
        );
        let square = Framing {
            aspect: Aspect::Square,
            x: 0.0,
            y: 1.0,
            zoom: 2.0,
        };
        assert_eq!(
            square.crop(1920, 1080),
            Some(Crop {
                x: 0,
                y: 540,
                width: 540,
                height: 540
            })
        );
    }

    #[test]
    fn cyrillic_title_changes_pixels_in_the_lower_part_of_the_crop() {
        let image = RgbaImage::from_pixel(320, 180, image::Rgba([18, 32, 51, 255]));
        let result = compose(&image, Framing::default(), "Первый кадр", &Fonts::new()).unwrap();
        assert_eq!(result.dimensions(), (320, 180));
        assert_eq!(result.get_pixel(0, 0), image.get_pixel(0, 0));
        assert!(result.pixels().any(|pixel| pixel.0 == [255, 255, 255, 255]));
    }
}
