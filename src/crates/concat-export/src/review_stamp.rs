// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use concat_core::frame::{Depth, Frame};

const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ],
];
const COLON: [u8; 7] = [0, 0, 0b00100, 0, 0b00100, 0, 0];
pub(super) const MIN_WIDTH: u32 = 90;
pub(super) const MIN_HEIGHT: u32 = 28;

fn clock(frame_number: i64, nominal_fps: i64) -> String {
    let nominal_fps = nominal_fps.max(1);
    let frame_number = frame_number.max(0);
    let elapsed = frame_number / nominal_fps;
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        elapsed / 3600,
        (elapsed / 60) % 60,
        elapsed % 60,
        frame_number % nominal_fps,
    )
}

pub(super) fn burn(frame: &mut Frame, frame_number: i64, nominal_fps: i64) {
    let [width, height] = [frame.width() as usize, frame.height() as usize];
    if width < MIN_WIDTH as usize || height < MIN_HEIGHT as usize || frame.depth() != Depth::Eight {
        return;
    }
    let text = clock(frame_number, nominal_fps);
    let columns = 6 * text.len() + 5;
    let margin = (width.min(height) / 40).max(2);
    let scale = (height / 190).max(1).min((width - margin * 2) / columns);
    let x = margin;
    let panel_width = columns * scale;
    let y = height - margin - 11 * scale;
    let pixels = frame.pixels_mut();

    for row in y..y + 11 * scale {
        let start = (row * width + x) * 4;
        let end = start + panel_width * 4;
        for pixel in pixels[start..end].chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel /= 4;
            }
            pixel[3] = 255;
        }
    }
    for (index, ch) in text.bytes().enumerate() {
        let shape = if ch == b':' {
            &COLON
        } else {
            &DIGITS[(ch - b'0') as usize]
        };
        for (row, bits) in shape.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                let left = x + (3 + index * 6 + column) * scale;
                let top = y + (2 + row) * scale;
                for py in top..top + scale {
                    let start = (py * width + left) * 4;
                    let end = start + scale * 4;
                    pixels[start..end].fill(255);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_uses_integer_frame_numbers() {
        assert_eq!(clock(29, 25), "00:00:01:04");
        assert_eq!(clock(90001, 25), "01:00:00:01");
    }

    #[test]
    fn review_changes_the_clock_when_a_new_frame_arrives() {
        let original = Frame::black(320, 180);
        let mut first = original.clone();
        let mut next = original.clone();
        burn(&mut first, 29, 25);
        burn(&mut next, 30, 25);
        assert_ne!(first.pixels(), next.pixels());
        assert_eq!(&first.pixels()[..320 * 4], &original.pixels()[..320 * 4]);
        assert!(
            first
                .pixels()
                .chunks_exact(4)
                .any(|pixel| pixel == [255; 4])
        );
    }
}
