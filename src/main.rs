use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use image::RgbImage;
use nokhwa::{
    pixel_format::RgbFormat,
    utils::{CameraIndex, RequestedFormat, RequestedFormatType},
    Camera,
};
use std::{path::PathBuf, thread, time::{Duration, Instant}};

mod ar;
mod image_pipeline;
mod nextface;
use ar::FaceTrack;
use image_pipeline::{auto_tune, enhance, Tuning};
use nextface::{FaceMesh, HeadPose};
#[cfg(windows)] mod virtual_camera;

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
    show_stats: bool,
    ar_enabled: bool,
    virtual_webcam: bool,
    last_process_ms: f32,
    auto_tune_enabled: bool,
    auto_tune_default_enabled: bool,
    auto_tune_interval_minutes: u32,
    last_auto_tune: Instant,
    face_tracks: Vec<FaceTrack>,
    nextface_mesh: Option<FaceMesh>,
    mesh_pose: Option<HeadPose>,
    ar_status: String,
    #[cfg(windows)]
    virtual_camera_publisher: Option<virtual_camera::VirtualCameraPublisher>,
    ar_tx: Sender<RgbImage>,
    ar_rx: Receiver<(Vec<FaceTrack>, String)>,
    nextface_root: String,
    nextface_python: String,
    nextface_status: String,
    nextface_tx: Sender<String>,
    nextface_rx: Receiver<String>,
    face_samples: Vec<(String, String)>,
    selected_face_sample: usize,
    face_sample_status: String,
}

impl CameraApp {
    fn new(rx: Receiver<CameraEvent>, camera_tx: Sender<CameraCommand>) -> Self {
        let (ar_tx, worker_rx) = bounded::<RgbImage>(1);
        let (worker_tx, ar_rx) = bounded::<(Vec<FaceTrack>, String)>(2);
        ar::spawn_worker(worker_rx, worker_tx);
        let (nextface_tx, nextface_rx) = bounded::<String>(2);
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
            show_stats: true,
            ar_enabled: false,
            virtual_webcam: false,
            last_process_ms: 0.0,
            auto_tune_enabled: false,
            auto_tune_default_enabled: false,
            auto_tune_interval_minutes: 2,
            last_auto_tune: Instant::now(),
            face_tracks: Vec::new(),
            nextface_mesh: None,
            mesh_pose: None,
            ar_status: "AR off".to_owned(),
            #[cfg(windows)]
            virtual_camera_publisher: virtual_camera::VirtualCameraPublisher::open().ok(),
            ar_tx,
            ar_rx,
            nextface_root: "NextFace".to_owned(),
            nextface_python: if cfg!(windows) { "python".to_owned() } else { "python3".to_owned() },
            nextface_status: "NextFace idle".to_owned(),
            nextface_tx,
            nextface_rx,
            face_samples: vec![
                ("Historic portrait — man".to_owned(), "https://commons.wikimedia.org/wiki/Special:Redirect/file/Portrait_of_a_man,_facing_front,_image_framed_by_gold_and_red_decorative_motif_LCCN2016653262.jpg".to_owned()),
                ("Historic portrait — woman".to_owned(), "https://commons.wikimedia.org/wiki/Special:Redirect/file/African_American_woman,_head-and-shoulders_portrait,_facing_front_LCCN99472177.jpg".to_owned()),
                ("Historic portrait — front view".to_owned(), "https://commons.wikimedia.org/wiki/Special:Redirect/file/Portrait_of_an_unidentified_man,_full-length,_standing,_facing_front_LCCN2015652128.jpg".to_owned()),
            ],
            selected_face_sample: 0,
            face_sample_status: "Samples are downloaded on demand.".to_owned(),
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

        while let Ok((tracks, status)) = self.ar_rx.try_recv() {
            self.face_tracks = tracks;
            self.ar_status = status;
        }
        while let Ok(status) = self.nextface_rx.try_recv() {
            self.nextface_status = status;
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
                self.face_tracks.clear();
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
                let live_text = if self.frozen { "PAUSED" } else { "LIVE" };
                let live_color = if self.frozen { egui::Color32::YELLOW } else { egui::Color32::LIGHT_GREEN };
                ui.label(egui::RichText::new(live_text).strong().color(live_color));
                ui.separator();
                ui.label(format!("{:.1} FPS", self.fps));
                ui.separator();
                ui.label(format!("{} face{}", self.face_tracks.len(), if self.face_tracks.len() == 1 { "" } else { "s" }));
                ui.separator();
                ui.label(if self.ar_enabled { "AR ON" } else { "AR OFF" });
                #[cfg(windows)]
                {
                    ui.separator();
                    ui.label(if self.virtual_webcam { "VIRTUAL CAMERA ON" } else { "VIRTUAL CAMERA OFF" });
                }
                if let Some(p) = &self.pair {
                    ui.separator();
                    ui.small(format!("capture {:.1} ms · enhance {:.1} ms", p.capture_ms, p.process_ms));
                }
                ui.separator();
                if ui.button(if self.compare { "Enhanced view" } else { "A/B compare" }).clicked() {
                    self.compare = !self.compare;
                }
                if ui.button(if self.frozen { "Resume" } else { "Freeze" }).clicked() {
                    self.frozen = !self.frozen;
                }
                ui.checkbox(&mut self.show_stats, "Stats");
            });
        });

        egui::SidePanel::left("controls")
            .resizable(true)
            .default_width(315.0)
            .min_width(250.0)
            .show(ctx, |ui| {
                ui.heading("Camera");
                ui.small("Capture → enhance → AR → virtual camera");
                ui.separator();
                ui.group(|ui| {
                    ui.strong("Quick controls");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Auto Enhance").clicked() {
                            if let Some(pair) = &self.pair { self.tuning = auto_tune(&pair.raw); }
                        }
                        if ui.button("Reset").clicked() { self.tuning = Tuning::default(); }
                        if ui.button(if self.compare { "Enhanced" } else { "Compare" }).clicked() { self.compare = !self.compare; }
                    });
                });
                ui.add_space(4.0);
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
                ui.collapsing("Image tuning", |ui| {
                ui.add(egui::Slider::new(&mut self.tuning.exposure, -1.0..=1.0).text("Exposure"));
                ui.add(egui::Slider::new(&mut self.tuning.contrast, 0.7..=1.5).text("Contrast"));
                ui.add(egui::Slider::new(&mut self.tuning.saturation, 0.5..=1.6).text("Saturation"));
                ui.add(egui::Slider::new(&mut self.tuning.sharpness, 0.0..=1.0).text("Detail"));
                ui.add(egui::Slider::new(&mut self.tuning.denoise, 0.0..=0.6).text("Denoise"));
                ui.add(egui::Slider::new(&mut self.tuning.warmth, -0.5..=0.5).text("Warmth"));
                ui.add(egui::Slider::new(&mut self.tuning.highlight_recovery, 0.0..=0.7).text("Highlights"));
                ui.add(egui::Slider::new(&mut self.tuning.shadow_lift, 0.0..=0.5).text("Shadows"));
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
                });
                ui.separator();
                ui.collapsing("Face AR", |ui| {
                    if ui.checkbox(&mut self.ar_enabled, "Enable face tracking").changed() {
                        if self.ar_enabled { self.ar_status = "Starting MediaPipe Face Landmarker…".to_owned(); }
                        else { self.face_tracks.clear(); self.ar_status = "AR off".to_owned(); }
                    }
                    ui.label(format!("Status: {}", self.ar_status));
                    ui.small("Realtime MediaPipe landmarks provide the low-latency tracking layer. NextFace reconstruction is separate because its optimization is much slower.");
                });
                ui.separator();
                #[cfg(windows)]
                ui.collapsing("Virtual camera", |ui| {
                    ui.checkbox(&mut self.virtual_webcam, "Publish enhanced frames");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Register").clicked() {
                            match virtual_camera::call_registration(true) {
                                Ok(()) => self.error = None,
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }
                        if ui.button("Unregister").clicked() {
                            match virtual_camera::call_registration(false) {
                                Ok(()) => self.virtual_webcam = false,
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }
                    });
                    ui.small("Frames published here are the same enhanced/AR-composited frames shown in the preview.");
                });

                ui.separator();
                ui.collapsing("NextFace · high-fidelity 3D reconstruction", |ui| {
                    ui.small("Runs NextFace on a captured RGB frame to reconstruct a textured 3D face mesh. This is an offline reconstruction backend, not a per-frame realtime effect.");
                    ui.horizontal(|ui| {
                        ui.label("NextFace");
                        ui.text_edit_singleline(&mut self.nextface_root);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Python");
                        ui.text_edit_singleline(&mut self.nextface_python);
                    });
                    if ui.button("Reconstruct current frame").clicked() {
                        self.nextface_mesh = None;
                        if let Some(pair) = &self.pair {
                            let root = PathBuf::from(self.nextface_root.trim());
                            let python = self.nextface_python.trim().to_owned();
                            let input = std::env::temp_dir().join("4k-rust-camera-nextface-input.png");
                            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                            let output = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("nextface-output").join(format!("reconstruction-{stamp}"));
                            match pair.raw.save(&input) {
                                Ok(()) => {
                                    self.nextface_status = format!("Starting NextFace… output: {}", output.display());
                                    let tx = self.nextface_tx.clone();
                                    thread::spawn(move || {
                                        let optimizer = root.join("optimizer.py");
                                        if !optimizer.exists() {
                                            let _ = tx.send(format!("NextFace not found: {}", optimizer.display()));
                                            return;
                                        }
                                        if let Err(e) = std::fs::create_dir_all(&output) {
                                            let _ = tx.send(format!("NextFace output directory failed: {e}"));
                                            return;
                                        }
                                        let result = std::process::Command::new(&python)
                                            .arg(&optimizer)
                                            .arg("--input").arg(&input)
                                            .arg("--output").arg(&output)
                                            .current_dir(&root)
                                            .spawn()
                                            .and_then(|mut child| child.wait());
                                        match result {
                                            Ok(status) if status.success() => {
                                                let _ = tx.send(format!("NextFace complete · mesh/output: {}", output.display()));
                                            }
                                            Ok(status) => {
                                                let _ = tx.send(format!("NextFace exited with {status}"));
                                            }
                                            Err(e) => {
                                                let _ = tx.send(format!("Could not start NextFace: {e}"));
                                            }
                                        }
                                    });
                                }
                                Err(e) => self.nextface_status = format!("Could not save frame: {e}"),
                            }
                        } else {
                            self.nextface_status = "Capture a frame before running reconstruction.".to_owned();
                        }
                    }
                    ui.label(format!("Status: {}", self.nextface_status));
                    ui.small("NextFace requires its Python environment plus the Basel morphable/albedo model files described by the upstream project.");
                });

                ui.separator();
                ui.collapsing("Face Samples · add to reconstruction", |ui| {
                    ui.small("Download public-domain sample portraits for testing the face-reconstruction pipeline. These are source images, not live 2D stickers.");
                    let mut requested = None;
                    egui::ComboBox::from_id_salt("face_sample_selector")
                        .selected_text(self.face_samples.get(self.selected_face_sample).map(|x| x.0.as_str()).unwrap_or("No sample"))
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for (i, (name, _)) in self.face_samples.iter().enumerate() {
                                if ui.selectable_label(i == self.selected_face_sample, name).clicked() { requested = Some(i); }
                            }
                        });
                    if let Some(i) = requested { self.selected_face_sample = i; }
                    if ui.button("Download selected sample").clicked() {
                        if let Some((name, url)) = self.face_samples.get(self.selected_face_sample).cloned() {
                            let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")) .join("assets").join("face-samples");
                            let safe = name.to_lowercase().replace(' ', "-").replace('—', "-").replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "");
                            let path = dir.join(format!("{safe}.jpg"));
                            self.face_sample_status = format!("Downloading {name}…");
                            let status_tx = self.nextface_tx.clone();
                            thread::spawn(move || {
                                let result = (|| -> Result<()> {
                                    std::fs::create_dir_all(&dir)?;
                                    let mut response = ureq::get(&url).call().map_err(|e| anyhow::anyhow!("sample download failed: {e}"))?;
                                    let bytes = response.body_mut().with_config().limit(12 * 1024 * 1024).read_to_vec().map_err(|e| anyhow::anyhow!("sample download failed: {e}"))?;
                                    if bytes.len() < 10_000 { anyhow::bail!("downloaded sample is unexpectedly small"); }
                                    std::fs::write(&path, bytes)?;
                                    Ok(())
                                })();
                                let _ = status_tx.send(match result { Ok(()) => format!("Sample ready: {}", path.display()), Err(e) => format!("Sample download failed: {e}") });
                            });
                        }
                    }
                    ui.label(format!("Status: {}", self.face_sample_status));
                    ui.small("The samples are public-domain historical portraits from Wikimedia Commons and are intended only as reconstruction test inputs.");
                });

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

                ui.collapsing("Face Mesh", |ui| {
                    ui.label("Realtime MediaPipe face mesh / landmarks");
                    ui.label(format!("{} tracked face(s), {} landmarks", self.face_tracks.len(), self.face_tracks.iter().map(|f| f.landmarks.len()).sum::<usize>()));
                    ui.small("MediaPipe: realtime landmarks. NextFace: high-fidelity reconstructed geometry.");
                    if let Some(mesh) = &self.nextface_mesh { ui.label(format!("NextFace mesh: {} vertices · {} triangles", mesh.vertices.len(), mesh.faces.len())); } else { ui.label("NextFace mesh: not loaded"); }
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
            ui.horizontal(|ui| {
                ui.strong(if self.compare { "Before / After" } else { "Live enhanced preview" });
                ui.add_space(8.0);
                ui.small(format!("{} × {}", self.pair.as_ref().map(|p| p.enhanced.width()).unwrap_or(0), self.pair.as_ref().map(|p| p.enhanced.height()).unwrap_or(0)));
                if self.ar_enabled {
                    ui.separator();
                    ui.small(&self.ar_status);
                }
            });
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
                    if self.ar_enabled && !self.face_tracks.is_empty() {
                        let rect = response.rect;
                        let sx = rect.width() / enhanced.size_vec2().x.max(1.0);
                        let sy = rect.height() / enhanced.size_vec2().y.max(1.0);
                        let painter = ui.painter_at(rect);
                        for track in &self.face_tracks {
                            let (x, y, w, h) = track.bbox;
                            let r = egui::Rect::from_min_size(rect.min + egui::vec2(x * sx, y * sy), egui::vec2(w * sx, h * sy));
                            painter.rect_stroke(r, 8.0, egui::Stroke::new(1.5_f32, egui::Color32::LIGHT_GREEN), egui::StrokeKind::Outside);
                            painter.text(r.left_top() + egui::vec2(4.0, 4.0), egui::Align2::LEFT_TOP, format!("FACE · {} landmarks", track.landmarks.len()), egui::TextStyle::Small.resolve(ui.style()), egui::Color32::WHITE);
                            for &(lx, ly, _lz) in &track.landmarks {
                                painter.circle_filled(rect.min + egui::vec2(lx * enhanced.size_vec2().x * sx, ly * enhanced.size_vec2().y * sy), 1.5, egui::Color32::LIGHT_GREEN);
                            }
                            if let Some(mesh) = &self.nextface_mesh { if let Some(track) = self.face_tracks.first() { self.mesh_pose = Some(nextface::estimate_head_pose(track)); } nextface::draw_wireframe(&painter, r, mesh, self.mesh_pose.unwrap_or_default()); }
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
    let selected = available[0].0.clone();
    let mut camera = Camera::new(selected.clone(), requested)?;
    camera.open_stream()?;

    loop {
        if let Ok(CameraCommand::Select(new_index)) = cmd_rx.try_recv() {
            camera.stop_stream().ok();
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
