use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use image::RgbImage;
use nokhwa::{
    pixel_format::RgbFormat,
    utils::{CameraIndex, RequestedFormat, RequestedFormatType},
    Camera,
};
use rayon::prelude::*;
use std::{thread, time::{Duration, Instant}};

#[derive(Clone, Debug)]
struct Tuning {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    sharpness: f32,
    denoise: f32,
    warmth: f32,
    highlight_recovery: f32,
    shadow_lift: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 1.08,
            saturation: 1.06,
            sharpness: 0.45,
            denoise: 0.15,
            warmth: 0.0,
            highlight_recovery: 0.25,
            shadow_lift: 0.08,
        }
    }
}

struct FramePair {
    raw: RgbImage,
    enhanced: RgbImage,
    capture_ms: f32,
    process_ms: f32,
}

enum CameraEvent {
    Frame(RgbImage, f32),
    Error(String),
}

struct CameraApp {
    rx: Receiver<CameraEvent>,
    tuning: Tuning,
    pair: Option<FramePair>,
    raw_texture: Option<egui::TextureHandle>,
    enhanced_texture: Option<egui::TextureHandle>,
    compare: bool,
    frozen: bool,
    last_frame: Instant,
    fps: f32,
    error: Option<String>,
    frames: u64,
    dropped_estimate: u64,
    show_stats: bool,
    active_section: u8,
    last_process_ms: f32,
}

impl CameraApp {
    fn new(rx: Receiver<CameraEvent>) -> Self {
        Self {
            rx,
            tuning: Tuning::default(),
            pair: None,
            raw_texture: None,
            enhanced_texture: None,
            compare: true,
            frozen: false,
            last_frame: Instant::now(),
            fps: 0.0,
            error: None,
            frames: 0,
            dropped_estimate: 0,
            show_stats: true,
            active_section: 0,
            last_process_ms: 0.0,
        }
    }

    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if let Some(pair) = &self.pair {
            let raw = egui::ColorImage::from_rgb(
                [pair.raw.width() as usize, pair.raw.height() as usize],
                pair.raw.as_raw(),
            );
            let enhanced = egui::ColorImage::from_rgb(
                [pair.enhanced.width() as usize, pair.enhanced.height() as usize],
                pair.enhanced.as_raw(),
            );

            if let Some(t) = &mut self.raw_texture {
                t.set(raw, egui::TextureOptions::LINEAR);
            } else {
                self.raw_texture = Some(ctx.load_texture("raw", raw, egui::TextureOptions::LINEAR));
            }

            if let Some(t) = &mut self.enhanced_texture {
                t.set(enhanced, egui::TextureOptions::LINEAR);
            } else {
                self.enhanced_texture =
                    Some(ctx.load_texture("enhanced", enhanced, egui::TextureOptions::LINEAR));
            }
        }
    }

    fn update_frame(&mut self, ctx: &egui::Context) {
        if self.frozen {
            return;
        }

        // Always process the newest available frame. This prevents the enhancement
        // pipeline from building visible latency when capture outruns processing.
        let mut latest = None;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                CameraEvent::Frame(raw, capture_ms) => latest = Some((raw, capture_ms)),
                CameraEvent::Error(message) => self.error = Some(message),
            }
        }

        if let Some((raw, capture_ms)) = latest {
            let start = Instant::now();
            let enhanced = enhance(&raw, &self.tuning);
            let process_ms = start.elapsed().as_secs_f32() * 1000.0;

            let dt = self.last_frame.elapsed().as_secs_f32().max(0.001);
            let instant_fps = 1.0 / dt;
            self.fps = if self.fps == 0.0 {
                instant_fps
            } else {
                0.9 * self.fps + 0.1 * instant_fps
            };
            self.last_frame = Instant::now();

            self.frames += 1;
            self.last_process_ms = process_ms;
            self.pair = Some(FramePair {
                raw,
                enhanced,
                capture_ms,
                process_ms,
            });
            self.refresh_textures(ctx);
        }
    }
}

impl eframe::App for CameraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_frame(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("4K Rust Camera");
                ui.separator();
                ui.label(egui::RichText::new("LIVE").strong().color(egui::Color32::LIGHT_GREEN));
                ui.separator();
                ui.label(format!("{:.1} FPS", self.fps));

                if let Some(p) = &self.pair {
                    ui.label(format!(
                        "capture {:.1} ms · enhance {:.1} ms",
                        p.capture_ms, p.process_ms
                    ));
                }

                ui.separator();
                ui.checkbox(&mut self.compare, "A/B compare");
                ui.checkbox(&mut self.frozen, "Freeze");
                ui.checkbox(&mut self.show_stats, "Stats");
            });
        });

        egui::SidePanel::left("controls")
            .resizable(true)
            .default_width(285.0)
            .min_width(250.0)
            .show(ctx, |ui| {
                ui.heading("Camera controls");
                ui.small("Live enhancement pipeline");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Source");
                    ui.monospace("Windows Camera #0");
                });
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    ui.monospace("Highest FPS");
                });
                ui.separator();
                ui.heading("Image tuning");
                ui.add(egui::Slider::new(&mut self.tuning.exposure, -1.0..=1.0).text("Exposure"));
                ui.add(egui::Slider::new(&mut self.tuning.contrast, 0.7..=1.5).text("Contrast"));
                ui.add(
                    egui::Slider::new(&mut self.tuning.saturation, 0.5..=1.6).text("Saturation"),
                );
                ui.add(egui::Slider::new(&mut self.tuning.sharpness, 0.0..=1.0).text("Detail"));
                ui.add(egui::Slider::new(&mut self.tuning.denoise, 0.0..=0.6).text("Denoise"));
                ui.add(egui::Slider::new(&mut self.tuning.warmth, -0.5..=0.5).text("Warmth"));
                ui.add(
                    egui::Slider::new(&mut self.tuning.highlight_recovery, 0.0..=0.7)
                        .text("Highlights"),
                );
                ui.add(
                    egui::Slider::new(&mut self.tuning.shadow_lift, 0.0..=0.5).text("Shadows"),
                );

                ui.horizontal(|ui| {
                    if ui.button("Reset").clicked() {
                        self.tuning = Tuning::default();
                    }
                    if ui.button("Neutral").clicked() {
                        self.tuning = Tuning { contrast: 1.0, saturation: 1.0, sharpness: 0.0, denoise: 0.0, ..Tuning::default() };
                    }
                });

                if false {
                    self.tuning = Tuning::default();
                }

                ui.separator();
                ui.collapsing("Performance", |ui| {
                    ui.label(format!("Frames processed: {}", self.frames));
                    ui.label(format!("Last enhancement: {:.1} ms", self.last_process_ms));
                    if let Some(p) = &self.pair {
                        ui.label(format!("Capture/decode: {:.1} ms", p.capture_ms));
                    }
                    ui.small("The newest frame is preferred so processing cannot build an unbounded queue.");
                });

                ui.collapsing("Pipeline", |ui| {
                ui.label("Low-latency CPU enhancement is applied to every frame.");
                ui.small("Frames are dropped intentionally when processing falls behind, keeping latency bounded.");

                });

                ui.collapsing("AI detail", |ui| {
                    ui.label("AI enhancement is intentionally disabled in the live 4K path until an ONNX/DirectML backend is benchmarked.");
                    ui.small("Planned: lightweight tiled inference with a latency guard, rather than forcing a heavy model over every 4K frame.");
                });

                if let Some(err) = &self.error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, format!("Camera: {err}"));
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            if let (Some(raw), Some(enhanced)) = (&self.raw_texture, &self.enhanced_texture) {
                if self.compare {
                    ui.columns(2, |cols| {
                        cols[0].label(egui::RichText::new("BASELINE").strong());
                        let avail = cols[0].available_size();
                        let ratio = raw.size_vec2().y / raw.size_vec2().x;
                        cols[0].image((raw.id(), egui::vec2(avail.x.max(1.0), (avail.x * ratio).min(avail.y * 0.88))));
                        cols[1].label(egui::RichText::new("ENHANCED").strong());
                        let avail = cols[1].available_size();
                        let ratio = enhanced.size_vec2().y / enhanced.size_vec2().x;
                        cols[1].image((enhanced.id(), egui::vec2(avail.x.max(1.0), (avail.x * ratio).min(avail.y * 0.88))));
                    });
                } else {
                    ui.label(egui::RichText::new("ENHANCED REALTIME FEED").strong());
                    let avail = ui.available_size();
                    let ratio = enhanced.size_vec2().y / enhanced.size_vec2().x;
                    ui.image((enhanced.id(), egui::vec2(avail.x.max(1.0), (avail.x * ratio).min(avail.y * 0.92))));
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.heading("Waiting for camera");
                    ui.label("Opening Windows camera #0 and negotiating the highest available frame rate…");
                    if let Some(err) = &self.error {
                        ui.colored_label(egui::Color32::RED, err);
                    }
                });
            }
        });

        ctx.request_repaint_after(Duration::from_millis(8));
    }
}

fn enhance(src: &RgbImage, t: &Tuning) -> RgbImage {
    let mut out = src.clone();
    let width = src.width() as usize;
    let height = src.height() as usize;

    out.as_mut()
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(i, p)| {
            let x = i % width;
            let y = i / width;
            let s = src.get_pixel(x as u32, y as u32);

            let mut r = s[0] as f32 / 255.0;
            let mut g = s[1] as f32 / 255.0;
            let mut b = s[2] as f32 / 255.0;

            let exposure = 2.0_f32.powf(t.exposure);
            r *= exposure;
            g *= exposure;
            b *= exposure;

            let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let shadow = (1.0 - lum).powi(2) * t.shadow_lift;
            let highlight = lum.max(0.0).powi(2) * t.highlight_recovery;

            r += shadow - highlight * r * 0.45;
            g += shadow - highlight * g * 0.45;
            b += shadow - highlight * b * 0.45;

            r = (r - 0.5) * t.contrast + 0.5;
            g = (g - 0.5) * t.contrast + 0.5;
            b = (b - 0.5) * t.contrast + 0.5;

            let lum2 = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            r = lum2 + (r - lum2) * t.saturation;
            g = lum2 + (g - lum2) * t.saturation;
            b = lum2 + (b - lum2) * t.saturation;

            r += t.warmth * 0.06;
            b -= t.warmth * 0.06;

            p[0] = (r.clamp(0.0, 1.0) * 255.0) as u8;
            p[1] = (g.clamp(0.0, 1.0) * 255.0) as u8;
            p[2] = (b.clamp(0.0, 1.0) * 255.0) as u8;
        });

    let original = out.clone();

    if width >= 3 && height >= 3 {
        out.as_mut()
            .par_chunks_mut(3)
            .enumerate()
            .for_each(|(i, p)| {
                let x = i % width;
                let y = i / width;
                if x == 0 || y == 0 || x + 1 >= width || y + 1 >= height {
                    return;
                }

                let c = original.get_pixel(x as u32, y as u32);
                let n = original.get_pixel(x as u32, (y - 1) as u32);
                let s = original.get_pixel(x as u32, (y + 1) as u32);
                let w = original.get_pixel((x - 1) as u32, y as u32);
                let e = original.get_pixel((x + 1) as u32, y as u32);

                for k in 0..3 {
                    let center = c[k] as f32;
                    let avg =
                        (n[k] as f32 + s[k] as f32 + w[k] as f32 + e[k] as f32) * 0.25;
                    let detail = center - avg;
                    let denoised = center * (1.0 - t.denoise) + avg * t.denoise;
                    p[k] = (denoised + detail * t.sharpness).clamp(0.0, 255.0) as u8;
                }
            });
    }

    out
}

fn camera_thread(tx: Sender<CameraEvent>) -> Result<()> {
    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = Camera::new(CameraIndex::Index(0), requested)?;

    camera.open_stream()?;

    loop {
        let start = Instant::now();
        let frame = camera.frame()?;
        let decoded = frame.decode_image::<RgbFormat>()?;
        let capture_ms = start.elapsed().as_secs_f32() * 1000.0;

        if tx.send(CameraEvent::Frame(decoded, capture_ms)).is_err() {
            break;
        }
    }

    Ok(())
}

fn main() {
    let (tx, rx) = bounded::<CameraEvent>(2);

    thread::spawn(move || {
        if let Err(e) = camera_thread(tx.clone()) {
            let _ = tx.send(CameraEvent::Error(format!("{e:#}")));
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("4K Rust Camera")
            .with_inner_size([1500.0, 900.0])
            .with_min_inner_size([1000.0, 650.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "4K Rust Camera",
        options,
        Box::new(|_cc| Ok(Box::new(CameraApp::new(rx)))),
    );
}
