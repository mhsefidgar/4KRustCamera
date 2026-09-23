use anyhow::Result;
use crossbeam_channel::{bounded, Receiver};
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
        Self { exposure: 0.0, contrast: 1.08, saturation: 1.06, sharpness: 0.45, denoise: 0.15,
               warmth: 0.0, highlight_recovery: 0.25, shadow_lift: 0.08 }
    }
}
struct FramePair { raw: RgbImage, enhanced: RgbImage, capture_ms: f32, process_ms: f32 }

struct CameraApp {
    rx: Receiver<(RgbImage, f32)>,
    tuning: Tuning,
    pair: Option<FramePair>,
    raw_texture: Option<egui::TextureHandle>,
    enhanced_texture: Option<egui::TextureHandle>,
    compare: bool,
    frozen: bool,
    last_frame: Instant,
    fps: f32,
    error: Option<String>,
}
impl CameraApp {
    fn new(rx: Receiver<(RgbImage, f32)>) -> Self {
        Self { rx, tuning: Tuning::default(), pair: None, raw_texture: None, enhanced_texture: None,
               compare: true, frozen: false, last_frame: Instant::now(), fps: 0.0, error: None }
    }
    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if let Some(pair) = &self.pair {
            let raw = egui::ColorImage::from_rgb([pair.raw.width() as usize, pair.raw.height() as usize], pair.raw.as_raw());
            let enhanced = egui::ColorImage::from_rgb([pair.enhanced.width() as usize, pair.enhanced.height() as usize], pair.enhanced.as_raw());
            if let Some(t) = &mut self.raw_texture { t.set(raw, egui::TextureOptions::LINEAR); }
            else { self.raw_texture = Some(ctx.load_texture("raw", raw, egui::TextureOptions::LINEAR)); }
            if let Some(t) = &mut self.enhanced_texture { t.set(enhanced, egui::TextureOptions::LINEAR); }
            else { self.enhanced_texture = Some(ctx.load_texture("enhanced", enhanced, egui::TextureOptions::LINEAR)); }
        }
    }
    fn update_frame(&mut self, ctx: &egui::Context) {
        if self.frozen { return; }
        if let Ok((raw, capture_ms)) = self.rx.try_recv() {
            let start = Instant::now();
            let enhanced = enhance(&raw, &self.tuning);
            let process_ms = start.elapsed().as_secs_f32() * 1000.0;
            let dt = self.last_frame.elapsed().as_secs_f32().max(0.001);
            self.fps = 0.9 * self.fps + 0.1 * (1.0 / dt);
            self.last_frame = Instant::now();
            self.pair = Some(FramePair { raw, enhanced, capture_ms, process_ms });
            self.refresh_textures(ctx);
        }
    }
}
impl eframe::App for CameraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_frame(ctx);
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("4K Rust Camera"); ui.separator();
                ui.label(format!("Realtime {:.1} FPS", self.fps));
                if let Some(p) = &self.pair { ui.label(format!("capture {:.1} ms · enhance {:.1} ms", p.capture_ms, p.process_ms)); }
                ui.checkbox(&mut self.compare, "Compare: baseline vs enhanced");
                ui.checkbox(&mut self.frozen, "Freeze");
            });
        });
        egui::SidePanel::left("controls").min_width(235.0).show(ctx, |ui| {
            ui.heading("Enhancement tuning");
            ui.add(egui::Slider::new(&mut self.tuning.exposure, -1.0..=1.0).text("Exposure"));
            ui.add(egui::Slider::new(&mut self.tuning.contrast, 0.7..=1.5).text("Contrast"));
            ui.add(egui::Slider::new(&mut self.tuning.saturation, 0.5..=1.6).text("Saturation"));
            ui.add(egui::Slider::new(&mut self.tuning.sharpness, 0.0..=1.0).text("Detail"));
            ui.add(egui::Slider::new(&mut self.tuning.denoise, 0.0..=0.6).text("Denoise"));
            ui.add(egui::Slider::new(&mut self.tuning.warmth, -0.5..=0.5).text("Warmth"));
            ui.add(egui::Slider::new(&mut self.tuning.highlight_recovery, 0.0..=0.7).text("Highlights"));
            ui.add(egui::Slider::new(&mut self.tuning.shadow_lift, 0.0..=0.5).text("Shadows"));
            if ui.button("Reset tuning").clicked() { self.tuning = Tuning::default(); }
            ui.separator();
            ui.heading("Lightweight AI");
            ui.label("The release packages a small Swin2SR x2 ONNX model for optional detail/snapshot experiments. The realtime 4K path stays model-free to keep latency low.");
            ui.small("Hugging Face: Xenova/swin2SR-lightweight-x2-64 family");
            if let Some(err) = &self.error { ui.colored_label(egui::Color32::RED, err); }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let (Some(raw), Some(enhanced)) = (&self.raw_texture, &self.enhanced_texture) {
                if self.compare {
                    ui.columns(2, |cols| {
                        cols[0].label("Baseline camera feed"); cols[0].image(raw);
                        cols[1].label("4K Rust Camera enhancement"); cols[1].image(enhanced);
                    });
                } else { ui.label("Enhanced feed"); ui.image(enhanced); }
            } else { ui.centered_and_justified(|ui| ui.label("Opening the first Windows camera…")); }
        });
        ctx.request_repaint_after(Duration::from_millis(8));
    }
}

fn enhance(src: &RgbImage, t: &Tuning) -> RgbImage {
    let mut out = src.clone();
    let width = src.width() as usize;
    let height = src.height() as usize;
    out.as_mut().par_chunks_mut(3).enumerate().for_each(|(i, p)| {
        let x = i % width; let y = i / width;
        let s = src.get_pixel(x as u32, y as u32);
        let mut r = s[0] as f32 / 255.0; let mut g = s[1] as f32 / 255.0; let mut b = s[2] as f32 / 255.0;
        let exposure = 2.0_f32.powf(t.exposure); r *= exposure; g *= exposure; b *= exposure;
        let lum = 0.2126*r + 0.7152*g + 0.0722*b;
        let shadow = (1.0-lum).powi(2) * t.shadow_lift;
        let highlight = lum.max(0.0).powi(2) * t.highlight_recovery;
        r += shadow - highlight*r*0.45; g += shadow - highlight*g*0.45; b += shadow - highlight*b*0.45;
        r = (r-0.5)*t.contrast+0.5; g = (g-0.5)*t.contrast+0.5; b = (b-0.5)*t.contrast+0.5;
        let lum2 = 0.2126*r + 0.7152*g + 0.0722*b;
        r = lum2 + (r-lum2)*t.saturation; g = lum2 + (g-lum2)*t.saturation; b = lum2 + (b-lum2)*t.saturation;
        r += t.warmth*0.06; b -= t.warmth*0.06;
        p[0]=(r.clamp(0.0,1.0)*255.0) as u8; p[1]=(g.clamp(0.0,1.0)*255.0) as u8; p[2]=(b.clamp(0.0,1.0)*255.0) as u8;
    });
    let original = out.clone();
    if width >= 3 && height >= 3 {
        out.as_mut().par_chunks_mut(3).enumerate().for_each(|(i,p)| {
            let x=i%width; let y=i/width; if x==0 || y==0 || x+1>=width || y+1>=height { return; }
            let c=original.get_pixel(x as u32,y as u32);
            let n=original.get_pixel(x as u32,(y-1) as u32); let s=original.get_pixel(x as u32,(y+1) as u32);
            let w=original.get_pixel((x-1) as u32,y as u32); let e=original.get_pixel((x+1) as u32,y as u32);
            for k in 0..3 {
                let center=c[k] as f32; let avg=(n[k] as f32+s[k] as f32+w[k] as f32+e[k] as f32)*0.25;
                let detail=center-avg; let denoised=center*(1.0-t.denoise)+avg*t.denoise;
                p[k]=(denoised+detail*t.sharpness).clamp(0.0,255.0) as u8;
            }
        });
    }
    out
}

fn camera_thread(tx: crossbeam_channel::Sender<(RgbImage, f32)>) -> Result<()> {
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = Camera::new(CameraIndex::Index(0), requested)?;
    camera.open_stream()?;
    loop {
        let start=Instant::now();
        let frame=camera.frame()?;
        let decoded=frame.decode_image::<RgbFormat>()?;
        let capture_ms=start.elapsed().as_secs_f32()*1000.0;
        if tx.send((decoded,capture_ms)).is_err() { break; }
    }
    Ok(())
}

fn main() -> Result<()> {
    let (tx,rx)=bounded::<(RgbImage,f32)>(2);
    thread::spawn(move || { if let Err(e)=camera_thread(tx) { eprintln!("Camera error: {e:#}"); } });
    let options=eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("4K Rust Camera").with_inner_size([1500.0,900.0]).with_min_inner_size([1000.0,650.0]),
        ..Default::default()
    };
    eframe::run_native("4K Rust Camera",options,Box::new(|_cc| Ok(Box::new(CameraApp::new(rx)))))?;
    Ok(())
}
