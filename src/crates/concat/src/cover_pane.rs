// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use image::{ImageEncoder, RgbaImage};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer};

use crate::cover_frame::{self, Aspect, Framing};
use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{App, CoverFrameUi};

#[derive(Default)]
struct State {
    epoch: u64,
    timeline: String,
    frame_id: u64,
    render_id: u64,
    source: Option<Arc<RgbaImage>>,
    output: Option<Arc<RgbaImage>>,
}

thread_local! {
    static COVER: RefCell<State> = RefCell::new(State::default());
}

pub(crate) fn bind(app: &App) {
    let ui = app.global::<CoverFrameUi>();
    ui.on_open_panel(|| {
        Shell::with(|shell, app| {
            open(&mut shell.studio.borrow_mut(), &app);
        })
    });
    ui.on_close(|| Shell::with(|_, app| close(&app)));
    ui.on_capture(|| {
        Shell::with(|shell, app| {
            request_frame(&mut shell.studio.borrow_mut(), &app);
        })
    });
    ui.on_refresh(|| Shell::with(|_, app| request_composition(&app)));
    ui.on_save(|| Shell::with(|shell, app| choose_file(&shell.studio.borrow(), &app)));
}

pub(crate) fn open(studio: &mut Studio, app: &App) {
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let ui = app.global::<CoverFrameUi>();
    if ui.get_open() {
        return;
    }
    COVER.with_borrow_mut(|state| {
        *state = State {
            epoch: host::project_epoch(),
            timeline: session.project().active_timeline_id.clone(),
            frame_id: state.frame_id.wrapping_add(1),
            render_id: state.render_id.wrapping_add(1),
            ..State::default()
        };
    });
    ui.set_open(true);
    ui.set_aspect(0);
    ui.set_horizontal(50.0);
    ui.set_vertical(50.0);
    ui.set_zoom(100.0);
    ui.set_title(SharedString::default());
    request_frame(studio, app);
}

fn close(app: &App) {
    let ui = app.global::<CoverFrameUi>();
    ui.set_open(false);
    ui.set_pending(false);
    ui.set_saving(false);
    ui.set_ready(false);
    ui.set_image(slint::Image::default());
    COVER.with_borrow_mut(|state| {
        state.source = None;
        state.output = None;
        state.frame_id = state.frame_id.wrapping_add(1);
        state.render_id = state.render_id.wrapping_add(1);
    });
}

pub(crate) fn publish(studio: &Studio, app: &App) {
    let ui = app.global::<CoverFrameUi>();
    ui.set_available(studio.live_enabled && studio.session.is_some() && studio.duration() > 0.0);
    if ui.get_open()
        && !COVER.with_borrow(|state| {
            state.epoch == host::project_epoch()
                && studio
                    .session
                    .as_ref()
                    .is_some_and(|session| session.project().active_timeline_id == state.timeline)
        })
    {
        close(app);
    }
}

fn request_frame(studio: &mut Studio, app: &App) {
    let ui = app.global::<CoverFrameUi>();
    if !ui.get_open() || ui.get_saving() {
        return;
    }
    let Some((clips, settings)) = studio.preview_clips() else {
        ui.set_status(t("cover.noFrame").into());
        return;
    };
    studio.pause();
    let (width, height) = studio.output_size();
    let scale = 1.0_f64.min(1920.0 / f64::from(width.max(height).max(1)));
    let dimension = |value: u32| ((f64::from(value) * scale).round() as u32).max(1);
    let [width, height] = [dimension(width), dimension(height)];
    let timeline = studio.timeline();
    let fps = timeline.video.rate();
    let last_frame = ((timeline.duration() * fps).ceil() - 1.0).max(0.0);
    let frame = (f64::from(studio.playhead) * fps)
        .round()
        .clamp(0.0, last_frame);
    let time = frame / fps;
    let timecode = crate::format::frame_count_timecode(frame as i64, fps.round() as i64);
    let spec = concat_host::preview::FrameSpec {
        width,
        height,
        time,
        moving: false,
        proxy: false,
        color_space: timeline.video.color_space,
        canvas: timeline.video.canvas,
        nearest: false,
    };
    let monitor = studio.host.monitor.clone();
    let frame_id = COVER.with_borrow_mut(|state| {
        state.source = None;
        state.output = None;
        state.frame_id = state.frame_id.wrapping_add(1);
        state.render_id = state.render_id.wrapping_add(1);
        state.frame_id
    });
    ui.set_ready(false);
    ui.set_pending(true);
    ui.set_status(t("cover.reading").into());
    host::spawn_in_project(
        move || {
            let bytes = monitor.frame(clips, &settings, spec).ok()?;
            RgbaImage::from_raw(width, height, bytes)
        },
        move |_, app, _, frame| {
            let ui = app.global::<CoverFrameUi>();
            if !ui.get_open() || COVER.with_borrow(|state| state.frame_id != frame_id) {
                return;
            }
            if let Some(frame) = frame {
                COVER.with_borrow_mut(|state| state.source = Some(Arc::new(frame)));
                ui.set_clock(timecode.into());
                request_composition(app);
            } else {
                ui.set_pending(false);
                ui.set_status(t("cover.frameError").into());
            }
        },
    );
}

fn request_composition(app: &App) {
    let ui = app.global::<CoverFrameUi>();
    let (Some(source), render_id) = COVER.with_borrow_mut(|state| {
        state.render_id = state.render_id.wrapping_add(1);
        state.output = None;
        (state.source.clone(), state.render_id)
    }) else {
        return;
    };
    ui.set_ready(false);
    ui.set_pending(true);
    let framing = Framing {
        aspect: match ui.get_aspect() {
            1 => Aspect::Wide,
            2 => Aspect::Square,
            3 => Aspect::Vertical,
            _ => Aspect::Original,
        },
        x: ui.get_horizontal() / 100.0,
        y: ui.get_vertical() / 100.0,
        zoom: ui.get_zoom() / 100.0,
    };
    let title = ui.get_title().to_string();
    Timer::single_shot(Duration::from_millis(100), move || {
        if COVER.with_borrow(|state| {
            state.render_id != render_id || state.epoch != host::project_epoch()
        }) {
            return;
        }
        host::spawn_in_project(
            move || cover_frame::compose(&source, framing, &title, &concat_text::Fonts::new()),
            move |_, app, _, result| {
                let ui = app.global::<CoverFrameUi>();
                if !ui.get_open() || COVER.with_borrow(|state| state.render_id != render_id) {
                    return;
                }
                ui.set_pending(false);
                match result {
                    Ok(image) => {
                        let (width, height) = image.dimensions();
                        let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                            image.as_raw(),
                            width,
                            height,
                        );
                        ui.set_image(slint::Image::from_rgba8(pixels));
                        ui.set_status(format!("{width} × {height}").into());
                        ui.set_ready(true);
                        COVER.with_borrow_mut(|state| state.output = Some(Arc::new(image)));
                    }
                    Err(cover_frame::CoverError::Frame) => {
                        ui.set_status(t("cover.frameError").into())
                    }
                    Err(cover_frame::CoverError::Title) => {
                        ui.set_status(t("cover.titleError").into())
                    }
                }
            },
        );
    });
}

fn choose_file(studio: &Studio, app: &App) {
    let ui = app.global::<CoverFrameUi>();
    if !ui.get_open() || !ui.get_ready() || ui.get_saving() {
        return;
    }
    let Some(session) = studio.session.as_ref() else {
        return;
    };
    let Some(image) = COVER.with_borrow(|state| state.output.clone()) else {
        return;
    };
    let directory = PathBuf::from(session.path());
    let epoch = host::project_epoch();
    let title = t("cover.save");
    let format = t("lib.pngImage");
    let suggested = concat_host::projects::folder_name(&studio.project_name) + "-cover.png";
    ui.set_saving(true);
    host::after_dialog(
        move || {
            Some(crate::platform::save_file(
                &title,
                &directory,
                &suggested,
                (&format, &["png"]),
            ))
        },
        move |_, app, _, file| {
            let ui = app.global::<CoverFrameUi>();
            if epoch != host::project_epoch() || !ui.get_open() {
                return;
            }
            let Some(file) = file else {
                ui.set_saving(false);
                return;
            };
            if !file
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
            {
                ui.set_saving(false);
                ui.set_status(t("cover.choosePng").into());
                return;
            }
            host::spawn_in_project(
                move || write_png(&file, &image).map(|()| file),
                |studio, app, _, result| {
                    let ui = app.global::<CoverFrameUi>();
                    ui.set_saving(false);
                    match result {
                        Ok(file) => studio.notify(&tf("cover.saved", &[&file.display()]), false),
                        Err(error) => ui.set_status(tf("cover.saveError", &[&error]).into()),
                    }
                },
            );
        },
    );
}

fn write_png(path: &Path, image: &RgbaImage) -> io::Result<()> {
    let name = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_file_name(format!(".concat-cover-{}-{name}.png", std::process::id()));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        let mut file = BufWriter::new(file);
        image::codecs::png::PngEncoder::new(&mut file)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(io::Error::other)?;
        file.flush()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
