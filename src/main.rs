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
    Cameras(Vec<(CameraIndex, String)>),
    Frame(RgbImage, f32),
    Error(String),
}

enum CameraCommand {
    Select(CameraIndex),
}

struct CameraApp {
    rx: Receiver<CameraEvent>,
    camera_tx: Sender<CameraCommand>,
    cameras: Vec<(CameraIndex, String)>,
    selected_camera: usize,
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
    ar_enabled: bool,
    virtual_webcam: bool,
    last_process_ms: f32,
}

impl CameraApp {
    fn new(rx: Receiver<CameraEvent>, camera_tx: Sender<CameraCommand>) -> Self {
        Self {
            rx,
            camera_tx,
            cameras: Vec::new(),
            selected_camera: 0,
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
            ar_enabled: false,
            virtual_webcam: false,
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
                CameraEvent::Cameras(cameras) => {
                    self.cameras = cameras;
                    if self.selected_camera >= self.cameras.len() {
                        self.selected_camera = 0;
                    }
                }
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
                ui.separator();
                if ui.button(if self.virtual_webcam { "Virtual Webcam: ON" } else { "Enable Virtual Webcam" }).clicked() {
                    self.virtual_webcam = !self.virtual_webcam;
                    self.error = Some("Virtual webcam integration requires the Windows 11 Media Foundation virtual-camera component; this button is reserved for the system camera bridge.".to_owned());
                }
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
                ui.label("Camera");
                if self.cameras.is_empty() {
                    ui.label("Detecting cameras…");
                } else {
                    let current_name = self
                        .cameras
                        .get(self.selected_camera)
                        .map(|(_, name)| name.as_str())
                        .unwrap_or("Unknown camera");
                    let mut requested = None;
                    egui::ComboBox::from_id_salt("camera_selector")
                        .selected_text(current_name)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for (index, (_, name)) in self.cameras.iter().enumerate() {
                                if ui.selectable_label(index == self.selected_camera, name).clicked() {
                                    requested = Some(index);
                                }
                            }
                        });
                    if let Some(index) = requested {
                        self.selected_camera = index;
                        if let Some((camera_index, _)) = self.cameras.get(index) {
                            let _ = self.camera_tx.send(CameraCommand::Select(camera_index.clone()));
                            self.pair = None;
                            self.raw_texture = None;
                            self.enhanced_texture = None;
                            self.error = None;
                        }
                    }
                }
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
                    if ui.button("Reset").clicked() { self.tuning = Tuning::default(); }
                    if ui.button("Neutral").clicked() { self.tuning = Tuning { contrast: 1.0, saturation: 1.0, sharpness: 0.0, denoise: 0.0, ..Tuning::default() }; }
                    if ui.button("Auto Tune").clicked() {
                        if let Some(pair) = &self.pair { self.tuning = auto_tune(&pair.raw); }
                    }
                });
                ui.separator();
                ui.heading("Augmented reality");
                ui.checkbox(&mut self.ar_enabled, "Face AR overlay");
                ui.small("Prepared for native face landmarks and glTF overlays; tracker integration is kept separate from the low-latency image path.");

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
                    ui.small("AR direction: MindAR-style face tracking with glTF assets adapted to the native Windows pipeline.");
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
                    ui.label("Select a camera above. The app negotiates the highest available frame rate.");
                    if let Some(err) = &self.error {
                        ui.colored_label(egui::Color32::RED, err);
                    }
                });
            }
        });

        ctx.request_repaint_after(Duration::from_millis(8));
    }
}

fn auto_tune(src: &RgbImage) -> Tuning {
    let mut sum = 0.0f64;
    let mut r_sum = 0.0f64;
    let mut b_sum = 0.0f64;
    let mut count = 0u64;
    for p in src.as_raw().chunks_exact(3) {
        let r = p[0] as f64 / 255.0;
        let g = p[1] as f64 / 255.0;
        let b = p[2] as f64 / 255.0;
        sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        r_sum += r;
        b_sum += b;
        count += 1;
    }
    if count == 0 { return Tuning::default(); }
    let mean = (sum / count as f64) as f32;
    let r_mean = (r_sum / count as f64) as f32;
    let b_mean = (b_sum / count as f64) as f32;
    Tuning {
        exposure: ((0.46 - mean) * 2.0).clamp(-0.7, 0.7),
        contrast: (1.10 + (0.46 - mean).abs() * 0.35).clamp(0.95, 1.25),
        saturation: 1.04,
        sharpness: 0.30,
        denoise: 0.12,
        warmth: ((r_mean - b_mean) * -0.8).clamp(-0.25, 0.25),
        highlight_recovery: 0.28,
        shadow_lift: ((0.40 - mean) * 0.30).clamp(0.02, 0.16),
    }
}

fn enhance(src: &RgbImage, t: &Tuning) -> RgbImage {
    let width = src.width() as usize;
    let height = src.height() as usize;
    let mut out = src.clone();
    let input = src.as_raw();
    let output = out.as_mut();
    let exposure = 2.0_f32.powf(t.exposure);
    output.par_chunks_mut(3).enumerate().for_each(|(i, p)| {
        let j = i * 3;
        let mut r = input[j] as f32 / 255.0 * exposure;
        let mut g = input[j + 1] as f32 / 255.0 * exposure;
        let mut b = input[j + 2] as f32 / 255.0 * exposure;
        let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let shadow = (1.0 - lum).powi(2) * t.shadow_lift;
        let highlight = lum.powi(2) * t.highlight_recovery;
        r += shadow - highlight * r * 0.45;
        g += shadow - highlight * g * 0.45;
        b += shadow - highlight * b * 0.45;
        r = (r - 0.5) * t.contrast + 0.5 + t.warmth * 0.06;
        g = (g - 0.5) * t.contrast + 0.5;
        b = (b - 0.5) * t.contrast + 0.5 - t.warmth * 0.06;
        let lum2 = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        r = lum2 + (r - lum2) * t.saturation;
        g = lum2 + (g - lum2) * t.saturation;
        b = lum2 + (b - lum2) * t.saturation;
        p[0] = (r.clamp(0.0, 1.0) * 255.0) as u8;
        p[1] = (g.clamp(0.0, 1.0) * 255.0) as u8;
        p[2] = (b.clamp(0.0, 1.0) * 255.0) as u8;
    });
    if width >= 3 && height >= 3 && (t.sharpness > 0.0 || t.denoise > 0.0) {
        let original = out.clone();
        let src_buf = original.as_raw();
        out.as_mut().par_chunks_mut(3).enumerate().for_each(|(i, p)| {
            let x = i % width;
            let y = i / width;
            if x == 0 || y == 0 || x + 1 >= width || y + 1 >= height { return; }
            let idx = (y * width + x) * 3;
            let north = idx - width * 3;
            let south = idx + width * 3;
            let west = idx - 3;
            let east = idx + 3;
            for k in 0..3 {
                let center = src_buf[idx + k] as f32;
                let avg = (src_buf[north + k] as f32 + src_buf[south + k] as f32 + src_buf[west + k] as f32 + src_buf[east + k] as f32) * 0.25;
                let detail = center - avg;
                let denoised = center * (1.0 - t.denoise) + avg * t.denoise;
                p[k] = (denoised + detail * t.sharpness).clamp(0.0, 255.0) as u8;
            }
        });
    }
    out
}

fn camera_thread(tx: Sender<CameraEvent>, cmd_rx: Receiver<CameraCommand>) -> Result<()> {
    let cameras = nokhwa::query(nokhwa::native_api_backend().ok_or_else(|| anyhow::anyhow!("No camera backend is available on this system."))?)?;
    let mut available = Vec::new();
    for info in cameras {
        available.push((info.index().clone(), info.human_name()));
    }
    let _ = tx.send(CameraEvent::Cameras(available.clone()));

    if available.is_empty() {
        return Err(anyhow::anyhow!("No cameras were detected."));
    }

    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut selected = available[0].0.clone();
    let mut camera = Camera::new(selected.clone(), requested)?;
    camera.open_stream()?;

    loop {
        if let Ok(CameraCommand::Select(new_index)) = cmd_rx.try_recv() {
            camera.stop_stream().ok();
            selected = new_index.clone();
            match Camera::new(new_index, RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate)) {
                Ok(mut new_camera) => match new_camera.open_stream() {
                    Ok(()) => camera = new_camera,
                    Err(e) => {
                        let _ = tx.send(CameraEvent::Error(format!("Could not open selected camera: {e:#}")));
                        continue;
                    }
                },
                Err(e) => {
                    let _ = tx.send(CameraEvent::Error(format!("Could not create selected camera: {e:#}")));
                    continue;
                }
            }
        }

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
    let (camera_tx, camera_rx) = bounded::<CameraCommand>(2);

    thread::spawn(move || {
        if let Err(e) = camera_thread(tx.clone(), camera_rx) {
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
        Box::new(|_cc| Ok(Box::new(CameraApp::new(rx, camera_tx)))),
    );
}
