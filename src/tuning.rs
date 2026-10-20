// SPDX-License-Identifier: MIT
//! The Tuning tab: the camera's picture, live, with snapshots, beside its image
//! controls. The camera keeps every change it accepts (it saves them itself
//! about two seconds after the last one), so there is nothing to save here.
//! Video and controls each run on a thread of their own, so a camera that
//! stops answering can't hold up the window.

use eframe::egui::{self, RichText};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::v4l2 as video;

/// Where a control lives: a standard V4L2 control (UVC Processing Unit or
/// Camera Terminal) or a selector on k1cam's Extension Unit 4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Std(u32),
    Xu(u8),
}

/// A control's current value, range and default, as the camera reports them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Value {
    pub value: i32,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    /// Has no effect now, e.g. the white balance temperature while white
    /// balance is automatic.
    pub inactive: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Group {
    Picture,
    Exposure,
    WhiteBalance,
    Noise,
    View,
    Encoder,
}

impl Group {
    const ALL: [Group; 6] = [Group::Picture, Group::Exposure, Group::WhiteBalance, Group::Noise, Group::View, Group::Encoder];

    fn title(self) -> &'static str {
        match self {
            Group::Picture => "Picture",
            Group::Exposure => "Exposure",
            Group::WhiteBalance => "White balance",
            Group::Noise => "Noise",
            Group::View => "View",
            Group::Encoder => "Encoder",
        }
    }
}

/// One control as uvcdctl knows it (its number and name are what `uvcdctl
/// get` and `set` take); ranges and defaults come from the camera.
struct Def {
    id: u32,
    name: &'static str,
    label: &'static str,
    group: Group,
    kind: Kind,
    help: &'static str,
    /// A choice: its values and what they mean.
    choices: &'static [(i32, &'static str)],
    /// An on/off control: what "on" means.
    check: &'static str,
    /// The value that means "automatic", if one does.
    auto: Option<i32>,
    suffix: &'static str,
}

const fn ctl(id: u32, name: &'static str, label: &'static str, group: Group, kind: Kind, help: &'static str) -> Def {
    Def { id, name, label, group, kind, help, choices: &[], check: "", auto: None, suffix: "" }
}

impl Def {
    const fn choices(mut self, choices: &'static [(i32, &'static str)]) -> Self {
        self.choices = choices;
        self
    }

    const fn check(mut self, check: &'static str) -> Self {
        self.check = check;
        self
    }

    const fn auto(mut self, auto: i32) -> Self {
        self.auto = Some(auto);
        self
    }

    const fn suffix(mut self, suffix: &'static str) -> Self {
        self.suffix = suffix;
        self
    }

    /// Changing it changes what other controls do or report (modes, choices):
    /// read them all again afterwards.
    fn governs(&self) -> bool {
        !self.choices.is_empty() || !self.check.is_empty()
    }
}

// V4L2_CID_BASE and V4L2_CID_CAMERA_CLASS_BASE
const STD: u32 = 0x0098_0900;
const CAM: u32 = 0x009a_0900;

use Group::*;

/// uvcdctl's controls (package/k1cam-uvcd/uvcdctl), grouped.
const DEFS: &[Def] = &[
    ctl(1, "brightness", "Brightness", Picture, Kind::Std(STD), "128: default"),
    ctl(2, "contrast", "Contrast", Picture, Kind::Std(STD + 1), "128: default"),
    ctl(3, "saturation", "Saturation", Picture, Kind::Std(STD + 2), "128: default"),
    ctl(5, "hue", "Hue", Picture, Kind::Std(STD + 3), "0: default"),
    ctl(4, "sharpness", "Sharpness", Picture, Kind::Std(STD + 27), "128: default; more made MJPEG frames too big for USB"),
    ctl(6, "gamma", "Gamma", Picture, Kind::Std(STD + 16), "Gamma x100; 100: the factory curve"),
    ctl(11, "auto-exposure", "Exposure", Exposure, Kind::Std(CAM + 1), "").choices(&[(3, "Automatic"), (1, "Manual")]),
    ctl(13, "exposure-time", "Exposure time", Exposure, Kind::Std(CAM + 2), "In 100 µs units (333 is 33.3 ms); manual exposure only"),
    ctl(12, "exposure-priority", "Exposure priority", Exposure, Kind::Std(CAM + 3), "Off: a constant frame rate")
        .check("frame rate may drop to expose longer"),
    ctl(20, "exposure-compensation", "Compensation", Exposure, Kind::Xu(3), "128: none; higher is brighter"),
    ctl(32, "metering", "Metering", Exposure, Kind::Xu(21), "Subject: what stands out bright, kept from clipping").choices(&[
        (0, "Tuning default"),
        (1, "Average"),
        (2, "Center-weighted"),
        (3, "Spot"),
        (4, "Subject"),
    ]),
    ctl(21, "max-analog-gain", "Max analog gain", Exposure, Kind::Xu(1), "32 per doubling: 160 is 32x (default), 224 the sensor's maximum"),
    ctl(22, "max-digital-gain", "Max digital gain", Exposure, Kind::Xu(2), "32 per doubling: 80 is about 5.7x (default)"),
    ctl(8, "power-line-frequency", "Anti-flicker", Exposure, Kind::Std(STD + 24), "For lights that flicker at the mains frequency")
        .choices(&[(0, "Off"), (1, "50 Hz"), (2, "60 Hz")]),
    ctl(28, "highlight-suppression", "Highlights", Exposure, Kind::Xu(9), "Highlight suppression: 0 off, 1-10 strength"),
    ctl(26, "dynamic-range-compression", "Range compression", Exposure, Kind::Xu(7), "Dynamic range compression; 128: as tuned"),
    ctl(9, "white-balance-auto", "Automatic", WhiteBalance, Kind::Std(STD + 12), "Off: set the temperature").check("on"),
    ctl(10, "white-balance-temperature", "Temperature", WhiteBalance, Kind::Std(STD + 26), "Manual white balance only").suffix(" K"),
    ctl(23, "spatial-denoise", "Spatial denoise", Noise, Kind::Xu(4), "128: as tuned"),
    ctl(24, "temporal-denoise", "Temporal denoise", Noise, Kind::Xu(5), "128: as tuned; high values smear moving things"),
    ctl(25, "defective-pixel-correction", "Pixel correction", Noise, Kind::Xu(6), "Defective pixel correction; 128: as tuned"),
    ctl(31, "rotation", "Rotation", View, Kind::Xu(22), "For a camera mounted upside down").choices(&[(0, "0°"), (180, "180°")]),
    ctl(14, "zoom", "Zoom", View, Kind::Std(CAM + 13), "Digital zoom x100; 100: the full view, at most 290 at 1920x1080"),
    ctl(15, "pan", "Pan", View, Kind::Std(CAM + 8), "Moves a zoomed view; positive is right"),
    ctl(16, "tilt", "Tilt", View, Kind::Std(CAM + 9), "Moves a zoomed view; positive is up"),
    ctl(50, "mjpeg-quality", "MJPEG quality", Encoder, Kind::Xu(19), "Higher is better (and bigger)"),
    ctl(40, "h264-bitrate-kbps", "H.264 bitrate", Encoder, Kind::Xu(13), "Target bitrate").auto(0).suffix(" kbps"),
    ctl(41, "h264-rate-control", "Rate control", Encoder, Kind::Xu(14), "H.264 rate control").choices(&[
        (0, "CBR"),
        (1, "VBR"),
        (2, "Capped VBR"),
    ]),
    ctl(43, "h264-min-qp", "H.264 min QP", Encoder, Kind::Xu(16), "Lowest QP, the best quality; automatic is 20").auto(-1),
    ctl(44, "h264-max-qp", "H.264 max QP", Encoder, Kind::Xu(17), "Highest QP, the worst quality").auto(-1),
    ctl(45, "h264-profile", "H.264 profile", Encoder, Kind::Xu(18), "").choices(&[(0, "Baseline"), (1, "Main"), (2, "High")]),
];

/// Elsewhere there is no V4L2; the tab says so.
#[cfg(not(target_os = "linux"))]
mod video {
    use super::{Kind, Value};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub fn node() -> Option<PathBuf> {
        None
    }
    pub fn frame_sizes(_: &Path) -> Result<Vec<(u32, u32)>, String> {
        Err("not available on this system".into())
    }
    pub struct Stream {
        pub width: u32,
        pub height: u32,
    }
    impl Stream {
        pub fn start(_: &Path, _: u32, _: u32, _: u32) -> Result<Self, String> {
            Err("not available on this system".into())
        }
        pub fn frame(&mut self, _: Duration) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }
    pub struct Controls;
    impl Controls {
        pub fn open(_: &Path) -> Result<Self, String> {
            Err("not available on this system".into())
        }
        pub fn read(&self, _: Kind) -> std::io::Result<Value> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        pub fn get(&self, _: Kind) -> std::io::Result<i32> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        pub fn write(&self, _: Kind, _: i32) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        pub fn factory_reset(&self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }
    pub fn unsupported(_: &std::io::Error) -> bool {
        false
    }
}

/// A decoded frame, with the JPEG it came from for snapshots.
struct Frame {
    jpeg: Vec<u8>,
    image: Arc<egui::ColorImage>,
    fps: f32,
}

enum StreamMsg {
    Started(u32, u32),
    Failed(String),
}

/// The capture thread and what it hands over.
struct Capture {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    rx: Receiver<StreamMsg>,
    thread: JoinHandle<()>,
    /// The size the camera agreed to, once streaming.
    size: Option<(u32, u32)>,
}

fn decode(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
    Some(egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw()))
}

fn capture(
    node: PathBuf,
    size: (u32, u32),
    fps: u32,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Frame>>>,
    tx: Sender<StreamMsg>,
    ctx: egui::Context,
) {
    let mut s = match video::Stream::start(&node, size.0, size.1, fps) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(StreamMsg::Failed(e));
            ctx.request_repaint();
            return;
        }
    };
    let _ = tx.send(StreamMsg::Started(s.width, s.height));
    let mut times: VecDeque<Instant> = VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        match s.frame(Duration::from_millis(250)) {
            Ok(Some(jpeg)) => {
                let now = Instant::now();
                times.push_back(now);
                while times.front().is_some_and(|t| now - *t > Duration::from_secs(2)) {
                    times.pop_front();
                }
                let fps = match (times.front(), times.len()) {
                    (Some(t), n) if n > 1 && now > *t => (n - 1) as f32 / (now - *t).as_secs_f32(),
                    _ => 0.0,
                };
                // A damaged frame is skipped; the next one replaces it.
                if let Some(image) = decode(&jpeg) {
                    *latest.lock().unwrap() = Some(Frame { jpeg, image: Arc::new(image), fps });
                    ctx.request_repaint();
                }
            }
            Ok(None) => {}
            Err(e) => {
                let _ = tx.send(StreamMsg::Failed(e));
                break;
            }
        }
    }
    ctx.request_repaint();
}

enum Req {
    Load,
    Set(usize, i32),
    Reset,
}

enum Resp {
    /// The MJPEG frame sizes the camera offers.
    Sizes(Result<Vec<(u32, u32)>, String>),
    Loaded(Result<Vec<Option<Value>>, String>),
    /// The value asked for, and what the camera then reports.
    Set(usize, i32, Result<i32, String>),
    Reset(Result<(), String>),
}

/// Every control's state; a control the camera doesn't have is None. The
/// first one that fails otherwise ends the reading, so that a camera that
/// doesn't answer costs one timeout, not one per control.
fn load_all(ctl: &video::Controls) -> Result<Vec<Option<Value>>, String> {
    let mut out = Vec::new();
    for d in DEFS {
        match ctl.read(d.kind) {
            Ok(v) => out.push(Some(v)),
            Err(e) if video::unsupported(&e) => out.push(None),
            Err(e) => return Err(format!("reading {}: {e}", d.name)),
        }
    }
    Ok(out)
}

/// Factory defaults (as `uvcdctl reset`): the camera resets every control,
/// but out of band, so uvcvideo keeps reporting the old standard values until
/// they are written. Writing each default back refreshes them; a control
/// governed by an automatic mode comes first, before that mode turns back on.
fn factory_reset(ctl: &video::Controls) -> Result<(), String> {
    ctl.factory_reset().map_err(|e| format!("factory reset: {e}"))?;
    for d in DEFS.iter().rev() {
        if let Kind::Std(_) = d.kind {
            if let Ok(v) = ctl.read(d.kind) {
                if !v.inactive {
                    let _ = ctl.write(d.kind, v.default);
                }
            }
        }
    }
    Ok(())
}

fn worker(node: PathBuf, rx: Receiver<Req>, tx: Sender<Resp>, ctx: egui::Context) {
    let ctl = match video::Controls::open(&node) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Resp::Loaded(Err(e)));
            ctx.request_repaint();
            return;
        }
    };
    let _ = tx.send(Resp::Sizes(video::frame_sizes(&node)));
    while let Ok(first) = rx.recv() {
        // Only the newest value of each control matters (a slider sends many).
        let (mut load, mut reset, mut sets) = (false, false, Vec::<(usize, i32)>::new());
        for r in std::iter::once(first).chain(rx.try_iter()) {
            match r {
                Req::Load => load = true,
                Req::Reset => reset = true,
                Req::Set(i, v) => {
                    sets.retain(|&(j, _)| j != i);
                    sets.push((i, v));
                }
            }
        }
        if reset {
            let _ = tx.send(Resp::Reset(factory_reset(&ctl)));
            load = true;
        }
        for (i, v) in sets {
            let d = &DEFS[i];
            let r = ctl.write(d.kind, v).and_then(|_| ctl.get(d.kind)).map_err(|e| e.to_string());
            // A refused value is read back, so that what shows is what applies.
            load |= d.governs() || r.is_err();
            let _ = tx.send(Resp::Set(i, v, r));
        }
        if load {
            let _ = tx.send(Resp::Loaded(load_all(&ctl)));
        }
        ctx.request_repaint();
    }
}

struct Worker {
    tx: Sender<Req>,
    rx: Receiver<Resp>,
}

/// The frame on screen.
struct Shown {
    jpeg: Vec<u8>,
    image: Arc<egui::ColorImage>,
    fps: f32,
    at: Instant,
}

pub struct Tuning {
    node: Option<PathBuf>,
    scanned: Option<Instant>,
    sizes: Vec<(u32, u32)>,
    size: (u32, u32),
    fps: u32,
    /// The preview should run (while the tab shows and the camera is there).
    live: bool,
    stream: Option<Capture>,
    /// A stopped capture thread, which must let go of the camera before the
    /// next one starts.
    retiring: Option<JoinHandle<()>>,
    retry: Instant,
    note: String,
    shown: Option<Shown>,
    texture: Option<egui::TextureHandle>,
    worker: Option<Worker>,
    vals: Vec<Option<Value>>,
    loaded: bool,
    ctl_error: Option<String>,
    confirm_reset: bool,
    log: Vec<String>,
}

const RATES: [u32; 6] = [30, 25, 20, 15, 10, 5];

impl Tuning {
    pub fn new() -> Self {
        Self {
            node: None,
            scanned: None,
            sizes: Vec::new(),
            size: (1280, 720),
            fps: 30,
            live: true,
            stream: None,
            retiring: None,
            retry: Instant::now(),
            note: String::new(),
            shown: None,
            texture: None,
            worker: None,
            vals: Vec::new(),
            loaded: false,
            ctl_error: None,
            confirm_reset: false,
            log: Vec::new(),
        }
    }

    /// Lines for the app's log (saved snapshots, resets).
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// Follows the camera, and runs the preview only while the tab shows.
    pub fn poll(&mut self, ctx: &egui::Context, visible: bool, camera_up: bool) {
        if !camera_up {
            if self.node.take().is_some() {
                self.worker = None;
                self.loaded = false;
            }
        } else if self.node.is_none() && self.scanned.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            self.scanned = Some(Instant::now());
            self.node = video::node();
        }
        self.poll_stream(ctx, visible);
        self.poll_controls(ctx, visible);
    }

    fn poll_stream(&mut self, ctx: &egui::Context, visible: bool) {
        if let Some(c) = &mut self.stream {
            let mut failed = None;
            loop {
                match c.rx.try_recv() {
                    Ok(StreamMsg::Started(w, h)) => {
                        c.size = Some((w, h));
                        self.note.clear();
                    }
                    Ok(StreamMsg::Failed(e)) => failed = Some(e),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            if let Some(f) = c.latest.lock().unwrap().take() {
                let opts = egui::TextureOptions::LINEAR;
                match &mut self.texture {
                    Some(t) => t.set(f.image.clone(), opts),
                    None => self.texture = Some(ctx.load_texture("preview", f.image.clone(), opts)),
                }
                self.shown = Some(Shown { jpeg: f.jpeg, image: f.image, fps: f.fps, at: Instant::now() });
            }
            if let Some(e) = failed {
                // Tried again in a moment: a camera that reboots comes back.
                self.note = e;
                self.retire();
                self.retry = Instant::now() + Duration::from_secs(3);
            }
        }
        let want = visible && self.live && self.node.is_some();
        let free = self.retiring.as_ref().is_none_or(|t| t.is_finished());
        if want && self.stream.is_none() && free && Instant::now() >= self.retry {
            self.start(ctx);
        } else if !want && self.stream.is_some() {
            self.retire();
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(node) = self.node.clone() else { return };
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let (size, fps, ctx) = (self.size, self.fps, ctx.clone());
        let (stop2, latest2) = (stop.clone(), latest.clone());
        let thread = std::thread::spawn(move || capture(node, size, fps, stop2, latest2, tx, ctx));
        self.retiring = None;
        self.stream = Some(Capture { stop, latest, rx, thread, size: None });
    }

    fn retire(&mut self) {
        if let Some(c) = self.stream.take() {
            c.stop.store(true, Ordering::Relaxed);
            self.retiring = Some(c.thread);
        }
    }

    fn restart(&mut self) {
        self.retire();
        self.retry = Instant::now();
    }

    fn poll_controls(&mut self, ctx: &egui::Context, visible: bool) {
        if visible && self.worker.is_none() && self.ctl_error.is_none() {
            if let Some(node) = self.node.clone() {
                let (tx, wrx) = channel();
                let (wtx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || worker(node, wrx, wtx, ctx));
                let _ = tx.send(Req::Load);
                self.worker = Some(Worker { tx, rx });
            }
        }
        let Some(w) = &self.worker else { return };
        let (mut lost, mut restart) = (false, false);
        loop {
            match w.rx.try_recv() {
                Ok(Resp::Sizes(Ok(sizes))) => {
                    if !sizes.contains(&self.size) {
                        self.size = sizes[0];
                        restart = true;
                    }
                    self.sizes = sizes;
                }
                Ok(Resp::Sizes(Err(e))) => self.note = e,
                Ok(Resp::Loaded(Ok(vals))) => {
                    self.vals = vals;
                    self.loaded = true;
                }
                Ok(Resp::Loaded(Err(e))) => {
                    self.ctl_error = Some(e);
                    lost = true;
                }
                Ok(Resp::Set(i, asked, r)) => match r {
                    // Unless the user has moved it on since.
                    Ok(v) => {
                        if let Some(val) = self.vals.get_mut(i).and_then(Option::as_mut).filter(|val| val.value == asked) {
                            val.value = v;
                        }
                    }
                    Err(e) => self.log.push(format!("Setting {} to {asked} failed: {e}", DEFS[i].name)),
                },
                Ok(Resp::Reset(r)) => self.log.push(match r {
                    Ok(()) => "Image controls reset to factory defaults".into(),
                    Err(e) => format!("FAILED: {e}"),
                }),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    lost = true;
                    break;
                }
            }
        }
        if lost {
            self.worker = None;
        }
        if restart {
            self.restart();
        }
    }

    fn send(&self, req: Req) {
        if let Some(w) = &self.worker {
            let _ = w.tx.send(req);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, camera_up: bool) {
        if cfg!(not(target_os = "linux")) {
            ui.label("The live preview and the image controls need Linux for now (V4L2).");
            return;
        }
        if !camera_up {
            ui.label(RichText::new("No camera running k1cam firmware is attached.").color(ui.visuals().weak_text_color()));
            return;
        }
        // The picture on the left, the controls in a column on the right.
        let full = ui.available_rect_before_wrap();
        let width = (full.width() * 0.42).clamp(300.0, 460.0);
        let (left, right) = full.split_left_right_at_x(full.right() - width);
        let gap = ui.spacing().item_spacing.x * 2.0;
        ui.scope_builder(egui::UiBuilder::new().max_rect(left.with_max_x(left.right() - gap)), |ui| self.preview_ui(ui));
        let x = left.right() - gap / 2.0;
        ui.painter().vline(x, full.y_range(), ui.visuals().widgets.noninteractive.bg_stroke);
        ui.scope_builder(egui::UiBuilder::new().max_rect(right.with_min_x(right.left() + gap / 2.0)), |ui| self.controls_ui(ui));
        ui.advance_cursor_after_rect(full);
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (label, tip) = if self.live {
                ("Pause", "Stop the preview, which frees the camera's video for other programs")
            } else {
                ("Live", "Start the preview")
            };
            if ui.button(label).on_hover_text(tip).clicked() {
                self.live = !self.live;
                self.retry = Instant::now();
            }
            let (size, fps) = (self.size, self.fps);
            egui::ComboBox::from_id_salt("preview-size").selected_text(format!("{}×{}", size.0, size.1)).show_ui(ui, |ui| {
                for &s in &self.sizes {
                    ui.selectable_value(&mut self.size, s, format!("{}×{}", s.0, s.1));
                }
            });
            egui::ComboBox::from_id_salt("preview-fps").width(70.0).selected_text(format!("{fps} fps")).show_ui(ui, |ui| {
                for r in RATES {
                    ui.selectable_value(&mut self.fps, r, format!("{r} fps"));
                }
            });
            if (self.size, self.fps) != (size, fps) {
                self.restart();
            }
            let have = self.shown.is_some();
            if ui
                .add_enabled(have, egui::Button::new("Snapshot…"))
                .on_hover_text("Save the frame on screen as the camera sent it (JPEG)")
                .clicked()
            {
                self.snapshot();
            }
            if ui.add_enabled(have, egui::Button::new("Copy")).on_hover_text("Copy the frame on screen").clicked() {
                if let Some(s) = &self.shown {
                    ui.ctx().copy_image((*s.image).clone());
                }
            }
        });
        ui.add_space(4.0);
        let avail = ui.available_size();
        let streaming = self.stream.as_ref().is_some_and(|c| c.size.is_some());
        let (Some(tex), Some(s)) = (&self.texture, &self.shown) else {
            let text = if self.node.is_none() {
                "The camera's video device was not found (/dev/video*).".into()
            } else if !self.note.is_empty() {
                self.note.clone()
            } else if self.live {
                "Starting the preview…".into()
            } else {
                "The preview is paused.".into()
            };
            ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(ui.visuals().weak_text_color())));
            return;
        };
        let [w, h] = s.image.size.map(|n| n as f32);
        let scale = (avail.x / w).min(avail.y / h).max(0.05);
        let size = egui::vec2(w * scale, h * scale);
        let rect =
            ui.allocate_ui_with_layout(avail, egui::Layout::top_down(egui::Align::Center), |ui| ui.image((tex.id(), size))).inner.rect;
        // What is shown, and why it may be stale.
        let mut caption = format!("{}×{}", w as u32, h as u32);
        if streaming && s.at.elapsed() < Duration::from_secs(2) {
            caption += &format!("  ·  {:.0} fps  ·  {} KiB per frame", s.fps, s.jpeg.len().div_ceil(1024));
        } else if !self.live {
            caption += "  ·  paused";
        } else if !self.note.is_empty() {
            caption += &format!("  ·  {}", self.note);
        } else {
            caption += "  ·  waiting for frames";
        }
        let painter = ui.painter_at(rect);
        let font = egui::FontId::proportional(12.0);
        let galley = painter.layout_no_wrap(caption, font, egui::Color32::WHITE);
        let at = rect.left_bottom() + egui::vec2(6.0, -6.0 - galley.size().y);
        painter.rect_filled(egui::Rect::from_min_size(at, galley.size()).expand(4.0), 4.0, egui::Color32::from_black_alpha(150));
        painter.galley(at, galley, egui::Color32::WHITE);
    }

    fn snapshot(&mut self) {
        let Some(s) = &self.shown else { return };
        let (jpeg, [w, h]) = (s.jpeg.clone(), s.image.size);
        let name = format!("k1cam-snapshot-{}.jpg", crate::ops::stamp());
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).add_filter("JPEG image", &["jpg", "jpeg"]).save_file() {
            self.log.push(match std::fs::write(&path, &jpeg) {
                Ok(()) => format!("Snapshot saved: {} ({w}×{h}, {} KiB)", path.display(), jpeg.len().div_ceil(1024)),
                Err(e) => format!("FAILED: {}: {e}", path.display()),
            });
        }
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Image controls").strong());
            if self.worker.is_some() && !self.loaded {
                ui.spinner();
            }
        });
        ui.label(
            RichText::new("The camera keeps every change; there is nothing to save. Hover over a control for what it does.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        if let Some(e) = self.ctl_error.clone() {
            ui.add_space(4.0);
            ui.colored_label(ui.visuals().warn_fg_color, format!("The camera's controls did not answer: {e}"));
            if ui.button("Try again").clicked() {
                self.ctl_error = None;
            }
        }
        ui.add_space(4.0);
        if !self.loaded {
            return;
        }
        let ready = self.worker.is_some();
        // Room for the labels, the value box and the scroll bar; the rest is slider.
        ui.spacing_mut().slider_width = (ui.available_width() - 235.0).max(60.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for g in Group::ALL {
                let rows: Vec<usize> =
                    (0..DEFS.len()).filter(|&i| DEFS[i].group == g && self.vals.get(i).is_some_and(Option::is_some)).collect();
                if rows.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(g.title()).default_open(g != Group::Encoder).show(ui, |ui| {
                    egui::Grid::new(g.title()).num_columns(2).min_col_width(118.0).spacing([10.0, 6.0]).show(ui, |ui| {
                        for i in rows {
                            self.control_row(ui, i, ready);
                            ui.end_row();
                        }
                    });
                });
            }
            ui.add_space(8.0);
            if self.confirm_reset {
                ui.label("Every control, the encoder settings included, goes back to its factory default.");
                ui.horizontal(|ui| {
                    if ui.button("Reset all").clicked() {
                        self.send(Req::Reset);
                        self.confirm_reset = false;
                    }
                    if ui.button("Keep them").clicked() {
                        self.confirm_reset = false;
                    }
                });
            } else if ui.add_enabled(ready, egui::Button::new("Factory defaults…")).clicked() {
                self.confirm_reset = true;
            }
        });
    }

    fn control_row(&mut self, ui: &mut egui::Ui, i: usize, ready: bool) {
        let d = &DEFS[i];
        let Some(val) = self.vals[i] else { return };
        let mut tip = format!("{} ({}), default {}", d.name, d.id, val.default);
        if !d.help.is_empty() {
            tip = format!("{}\n\n{tip}", d.help);
        }
        // A changed control stands out, and its name puts the default back.
        let changed = val.value != val.default;
        let label = if changed { RichText::new(d.label).color(ui.visuals().selection.stroke.color) } else { RichText::new(d.label) };
        let r = ui.add(egui::Label::new(label).sense(if changed && ready && !val.inactive {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        }));
        let mut v = val.value;
        let r = r.on_hover_text(if changed { format!("{tip}\n\nClick to go back to the default.") } else { tip });
        if changed && r.clicked() {
            v = val.default;
        }
        ui.add_enabled_ui(ready && !val.inactive, |ui| {
            if !d.choices.is_empty() {
                let shown = d.choices.iter().find(|c| c.0 == v).map_or(v.to_string(), |c| c.1.to_string());
                egui::ComboBox::from_id_salt(("ctl", d.id)).selected_text(shown).show_ui(ui, |ui| {
                    for &(cv, cl) in d.choices.iter().filter(|c| (val.min..=val.max).contains(&c.0)) {
                        ui.selectable_value(&mut v, cv, cl);
                    }
                });
            } else if !d.check.is_empty() {
                let mut on = v != 0;
                ui.checkbox(&mut on, d.check);
                v = on as i32;
            } else {
                let mut s = egui::Slider::new(&mut v, val.min..=val.max).step_by(val.step as f64).suffix(d.suffix);
                if let Some(auto) = d.auto {
                    s = s
                        .custom_formatter(move |n, _| if n as i32 == auto { "auto".into() } else { format!("{n:.0}") })
                        .custom_parser(move |t| if t.trim() == "auto" { Some(auto as f64) } else { t.trim().parse().ok() });
                }
                ui.add(s);
            }
        })
        .response
        .on_disabled_hover_text(if val.inactive { "Has no effect in the current mode" } else { "" });
        if v != val.value {
            if let Some(x) = self.vals[i].as_mut() {
                x.value = v;
            }
            self.send(Req::Set(i, v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_uvcdctls() {
        // Numbers and names are what uvcdctl takes: never two the same.
        for (i, a) in DEFS.iter().enumerate() {
            for b in &DEFS[i + 1..] {
                assert!(a.id != b.id && a.name != b.name && a.kind != b.kind, "{} and {}", a.name, b.name);
            }
        }
        // Groups are listed together, in the order they are shown.
        let order: Vec<usize> = DEFS.iter().map(|d| Group::ALL.iter().position(|g| *g == d.group).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }
}
