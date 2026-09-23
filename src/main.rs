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
use std::{path::PathBuf, thread, time::{Duration, Instant}};
#[cfg(windows)] mod virtual_camera;
use mediapipe::{FaceDetector, Image as MpImage, ModelSource, IouThreshold};

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
    auto_tune_enabled: bool,
    auto_tune_default_enabled: bool,
    auto_tune_interval_minutes: u32,
    last_auto_tune: Instant,
    face_boxes: Vec<(f32, f32, f32, f32, f32)>,
    face_landmarks: Vec<Vec<[f32; 3]>>,
    ar_layout: usize,
    ar_object: usize,
    ar_scale: f32,
    ar_status: String,
    #[cfg(windows)]
    virtual_camera_publisher: Option<virtual_camera::VirtualCameraPublisher>,
    ar_tx: Sender<RgbImage>,
    ar_rx: Receiver<(Vec<(f32, f32, f32, f32, f32)>, String)>,
}

impl CameraApp {
    fn new(rx: Receiver<CameraEvent>, camera_tx: Sender<CameraCommand>) -> Self {
        let (ar_tx, worker_rx) = bounded::<RgbImage>(1);
        let (worker_tx, ar_rx) = bounded::<(Vec<(f32, f32, f32, f32, f32)>, String)>(2);
        thread::spawn(move || face_ai_worker(worker_rx, worker_tx));
        Self {
            rx,
            camera_tx,
            cameras: Vec::new(),
            selected_camera: 0,
            tuning: Tuning::default(),
            pair: None,
            raw_texture: None,
            enhanced_texture: None,
            compare: false,
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
            auto_tune_enabled: false,
            auto_tune_default_enabled: false,
            auto_tune_interval_minutes: 2,
            last_auto_tune: Instant::now(),
            face_boxes: Vec::new(),
            face_landmarks: Vec::new(),
            ar_layout: 0,
            ar_object: 0,
            ar_scale: 1.0,
            ar_status: "AR off".to_owned(),
            #[cfg(windows)]
            virtual_camera_publisher: virtual_camera::VirtualCameraPublisher::open().ok(),
            ar_tx,
            ar_rx,
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

        while let Ok((boxes, status)) = self.ar_rx.try_recv() {
            self.face_boxes = boxes;
            self.ar_status = status;
        }

        if let Some((raw, capture_ms)) = latest {
            if self.auto_tune_enabled && self.last_auto_tune.elapsed() >= Duration::from_secs(self.auto_tune_interval_minutes.max(1) as u64 * 60) {
                self.tuning = auto_tune(&raw);
                self.last_auto_tune = Instant::now();
            }
            let start = Instant::now();
            let enhanced = enhance(&raw, &self.tuning);
            #[cfg(windows)]
            if self.virtual_webcam {
                if let Some(publisher) = &mut self.virtual_camera_publisher {
                    if let Err(e) = publisher.publish(&enhanced) { self.error = Some(format!("Virtual camera IPC: {e:#}")); }
                }
            }
            let process_ms = start.elapsed().as_secs_f32() * 1000.0;
            if self.ar_enabled {
                let _ = self.ar_tx.try_send(raw.clone());
            } else {
                self.face_boxes.clear();
                self.ar_status = "AR off".to_owned();
            }

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
                ui.heading("Camera");
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
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Output");
                    ui.small(if self.compare { "A/B comparison" } else { "Enhanced only" });
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

                ui.horizontal_wrapped(|ui| {
                    if ui.button("Auto Tune").clicked() {
                        if let Some(pair) = &self.pair { self.tuning = auto_tune(&pair.raw); }
                    }
                    if ui.button("Reset").clicked() { self.tuning = Tuning::default(); }
                    if ui.button("Neutral").clicked() {
                        self.tuning = Tuning { contrast: 1.0, saturation: 1.0, sharpness: 0.0, denoise: 0.0, ..Tuning::default() };
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.auto_tune_enabled, "Auto Tune every");
                    ui.add_enabled(self.auto_tune_enabled, egui::DragValue::new(&mut self.auto_tune_interval_minutes).range(1..=60).suffix(" min"));
                });
                ui.checkbox(&mut self.auto_tune_default_enabled, "Use Auto Tune by default");
                if self.auto_tune_enabled && self.last_auto_tune.elapsed() >= Duration::from_secs(self.auto_tune_interval_minutes.max(1) as u64 * 60) {
                    self.last_auto_tune = Instant::now();
                }
                ui.small("Automatic tuning updates the image parameters from the newest frame at the selected interval. Default interval: 2 minutes.");
                ui.separator();
                ui.heading("AR & virtual camera");
                if ui.checkbox(&mut self.ar_enabled, "Face AR overlay").changed() {
                    if self.ar_enabled { self.ar_status = "Starting MediaPipe face detector…".to_owned(); }
                    else { self.face_boxes.clear(); self.ar_status = "AR off".to_owned(); }
                }
                ui.label(format!("Status: {}", self.ar_status));
                if self.ar_enabled {
                    ui.horizontal(|ui| {
                        ui.label("AR layout");
                        egui::ComboBox::from_id_salt("ar_layout")
                            .selected_text(match self.ar_layout { 1 => "3D Effects", _ => "Face AR" })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.ar_layout, 0, "Face AR");
                                ui.selectable_value(&mut self.ar_layout, 1, "3D Effects");
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("3D effect");
                        egui::ComboBox::from_id_salt("ar_object")
                            .selected_text(match self.ar_object { 1 => "Glasses", 2 => "Crown", 3 => "Cube", _ => "None" })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.ar_object, 0, "None");
                                ui.selectable_value(&mut self.ar_object, 1, "Glasses");
                                ui.selectable_value(&mut self.ar_object, 2, "Crown");
                                ui.selectable_value(&mut self.ar_object, 3, "Cube");
                            });
                    });
                    if self.ar_object != 0 {
                        ui.add(egui::Slider::new(&mut self.ar_scale, 0.5..=1.8).text("3D effect scale"));
                    }
                }
                ui.small("MediaPipe BlazeFace runs on a reduced preview frame; the 4K enhancement path is not replaced.");
                #[cfg(windows)] {
                    ui.checkbox(&mut self.virtual_webcam, "Publish enhanced frames to virtual camera");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Register virtual camera").clicked() {
                            match virtual_camera::call_registration(true) { Ok(()) => self.error=None, Err(e) => self.error=Some(format!("{e:#}")) }
                        }
                        if ui.button("Unregister virtual camera").clicked() {
                            match virtual_camera::call_registration(false) { Ok(()) => self.virtual_webcam=false, Err(e) => self.error=Some(format!("{e:#}")) }
                        }
                    });
                }
                ui.small("The virtual camera uses a Windows Media Foundation custom source backed by a shared-memory frame ring.");

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
                    ui.label(egui::RichText::new("ENHANCED · REALTIME").strong());
                    let avail = ui.available_size();
                    let ratio = enhanced.size_vec2().y / enhanced.size_vec2().x;
                    let size = egui::vec2(avail.x.max(1.0), (avail.x * ratio).min(avail.y * 0.92));
                    let response = ui.image((enhanced.id(), size));
                    if self.ar_enabled && !self.face_boxes.is_empty() {
                        let rect = response.rect;
                        let sx = rect.width() / enhanced.size_vec2().x.max(1.0);
                        let sy = rect.height() / enhanced.size_vec2().y.max(1.0);
                        let painter = ui.painter_at(rect);
                        for (x, y, w, h, score) in &self.face_boxes {
                            let r = egui::Rect::from_min_size(rect.min + egui::vec2(*x * sx, *y * sy), egui::vec2(*w * sx, *h * sy));
                            painter.rect_stroke(r, 8.0, egui::Stroke::new(2.0, egui::Color32::LIGHT_GREEN), egui::StrokeKind::Outside);
                            painter.text(r.left_top() + egui::vec2(4.0, 4.0), egui::Align2::LEFT_TOP, format!("FACE {:.0}%", score * 100.0), egui::TextStyle::Small.resolve(ui.style()), egui::Color32::WHITE);
                            draw_ar_object(&painter, r, self.ar_object, self.ar_scale);
                        }
                    }
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


fn face_ai_worker(rx: Receiver<RgbImage>, tx: Sender<(Vec<(f32, f32, f32, f32, f32)>, String)>) {
    let model = model_path();
    if !model.exists() {
        let _ = tx.send((Vec::new(), "AR: downloading lightweight face model…".to_owned()));
        if let Err(e) = download_face_model(&model) {
            let _ = tx.send((Vec::new(), format!("AR unavailable: {e}")));
            return;
        }
    }
    let mut detector = match FaceDetector::builder(ModelSource::path(&model))
        .min_detection_confidence(mediapipe::Confidence::new(0.5).expect("valid confidence"))
        .min_suppression_threshold(IouThreshold::new(0.3).expect("valid IoU threshold"))
        .build()
    {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send((Vec::new(), format!("AR model load failed: {e}")));
            return;
        }
    };
    let _ = tx.send((Vec::new(), "AR ready".to_owned()));
    while let Ok(src) = rx.recv() {
        let max_w = 640u32;
        let small = if src.width() > max_w {
            let h = ((src.height() as f32) * max_w as f32 / src.width() as f32) as u32;
            image::imageops::resize(&src, max_w, h.max(1), image::imageops::FilterType::Triangle)
        } else {
            src.clone()
        };
        let temp = std::env::temp_dir().join("4k-rust-camera-ar.png");
        if let Err(e) = small.save(&temp) {
            let _ = tx.try_send((Vec::new(), format!("AR frame preparation failed: {e}")));
            continue;
        }
        let result: Result<Vec<(f32, f32, f32, f32, f32)>> = (|| {
            let image = MpImage::from_file(&temp)?;
            let detections = detector.detect(&image)?;
            let sx = src.width() as f32 / small.width().max(1) as f32;
            let sy = src.height() as f32 / small.height().max(1) as f32;
            Ok(detections.into_iter().map(|face| {
                let bb = face.bounding_box;
                let score = face.score().map(|v| v.get()).unwrap_or(0.0);
                (
                    bb.left() as f32 * sx,
                    bb.top() as f32 * sy,
                    bb.width() as f32 * sx,
                    bb.height() as f32 * sy,
                    score,
                )
            }).collect())
        })();
        let _ = std::fs::remove_file(&temp);
        match result {
            Ok(boxes) => {
                let count = boxes.len();
                let _ = tx.try_send((boxes, format!("AR active · {} face(s)", count)));
            }
            Err(e) => {
                let _ = tx.try_send((Vec::new(), format!("AR inference failed: {e}")));
            }
        }
    }
}

fn download_face_model(path: &PathBuf) -> Result<()> {
    const URL: &str = "https://storage.googleapis.com/mediapipe-models/face_detector/blaze_face_short_range/float16/1/blaze_face_short_range.tflite";
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let mut response = ureq::get(URL).call().map_err(|e| anyhow::anyhow!("model download failed: {e}"))?;
    let bytes = response.body_mut().with_config().limit(2 * 1024 * 1024).read_to_vec().map_err(|e| anyhow::anyhow!("model download failed: {e}"))?;
    if bytes.len() < 100_000 { anyhow::bail!("downloaded model is unexpectedly small"); }
    let temp = path.with_extension("part");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

fn model_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    p.pop(); p.push("models"); p.push("blaze_face_short_range.tflite"); p
}
fn draw_ar_object(painter: &egui::Painter, r: egui::Rect, object: usize, scale: f32) {
    if object == 0 { return; }
    let cx = r.center().x;
    let cy = r.top() + r.height() * 0.46;
    let w = r.width() * scale;
    let h = r.height() * scale;
    match object {
        1 => {
            let lens_w = w * 0.28;
            let lens_h = h * 0.14;
            let gap = w * 0.045;
            let left = egui::Rect::from_center_size(
                egui::pos2(cx - lens_w - gap, cy),
                egui::vec2(lens_w, lens_h),
            );
            let right = egui::Rect::from_center_size(
                egui::pos2(cx + lens_w + gap, cy),
                egui::vec2(lens_w, lens_h),
            );
            painter.rect_stroke(left, 8.0, egui::Stroke::new(3.0, egui::Color32::WHITE), egui::StrokeKind::Outside);
            painter.rect_stroke(right, 8.0, egui::Stroke::new(3.0, egui::Color32::WHITE), egui::StrokeKind::Outside);
            painter.line_segment([egui::pos2(left.right(), cy), egui::pos2(right.left(), cy)], egui::Stroke::new(3.0, egui::Color32::WHITE));
            painter.line_segment([egui::pos2(left.left(), cy), egui::pos2(left.left() - w * 0.08, cy - h * 0.04)], egui::Stroke::new(3.0, egui::Color32::WHITE));
            painter.line_segment([egui::pos2(right.right(), cy), egui::pos2(right.right() + w * 0.08, cy - h * 0.04)], egui::Stroke::new(3.0, egui::Color32::WHITE));
        }
        2 => {
            let base_y = r.top() - h * 0.05;
            let pts = [
                egui::pos2(cx - w * 0.34, base_y),
                egui::pos2(cx - w * 0.18, base_y - h * 0.22),
                egui::pos2(cx - w * 0.03, base_y),
                egui::pos2(cx + w * 0.08, base_y - h * 0.30),
                egui::pos2(cx + w * 0.22, base_y),
                egui::pos2(cx + w * 0.38, base_y - h * 0.16),
                egui::pos2(cx + w * 0.42, base_y),
            ];
            for pair in pts.windows(2) {
                painter.line_segment([pair[0], pair[1]], egui::Stroke::new(4.0, egui::Color32::WHITE));
            }
            painter.line_segment([pts[0], pts[6]], egui::Stroke::new(4.0, egui::Color32::WHITE));
        }
        3 => {
            let depth = w * 0.10;
            let top = egui::pos2(cx, cy - h * 0.20);
            let left = egui::pos2(cx - w * 0.24, cy - h * 0.08);
            let right = egui::pos2(cx + w * 0.24, cy - h * 0.08);
            let bottom = egui::pos2(cx, cy + h * 0.22);
            painter.line_segment([top, left], egui::Stroke::new(3.0, egui::Color32::WHITE));
            painter.line_segment([top, right], egui::Stroke::new(3.0, egui::Color32::WHITE));
            painter.line_segment([left, bottom], egui::Stroke::new(3.0, egui::Color32::WHITE));
            painter.line_segment([right, bottom], egui::Stroke::new(3.0, egui::Color32::WHITE));
            let o = egui::vec2(depth, -depth);
            for (a, b) in [(top, left), (top, right), (left, bottom), (right, bottom)] {
                painter.line_segment([a + o, b + o], egui::Stroke::new(2.0, egui::Color32::WHITE));
            }
            for p in [top, left, right, bottom] {
                painter.line_segment([p, p + o], egui::Stroke::new(2.0, egui::Color32::WHITE));
            }
        }
        _ => {}
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
