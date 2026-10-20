// SPDX-License-Identifier: MIT
mod bundle;
mod cloner;
mod job;
mod xu;

use eframe::egui;
use job::{Event, Job};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FLASH_SIZE: usize = 8 * 1024 * 1024;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Cloner,
    Update,
    Help,
}

struct App {
    tab: Tab,
    log: Vec<String>,
    progress: f32,
    rx: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    cloner_up: bool,
    camera_up: bool,
    last_poll: Instant,
    image: Option<PathBuf>,
    backup_first: bool,
    verify: bool,
    reboot_after: bool,
    confirm: bool,
    bundle: Option<PathBuf>,
    reboot_trial: bool,
    live_dump: bool,
}

impl App {
    fn new() -> Self {
        Self {
            tab: Tab::Cloner,
            log: vec!["Ready.".into()],
            progress: 0.0,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            cloner_up: false,
            camera_up: false,
            last_poll: Instant::now() - Duration::from_secs(10),
            image: None,
            backup_first: true,
            verify: true,
            reboot_after: true,
            confirm: false,
            bundle: None,
            reboot_trial: true,
            live_dump: false,
        }
    }

    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn start<F>(&mut self, name: &str, f: F)
    where
        F: FnOnce(&Job) -> Result<String, String> + Send + 'static,
    {
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.cancel.store(false, Ordering::Relaxed);
        self.progress = 0.0;
        self.log.push(format!("--- {name} ---"));
        let job = Job::new(tx.clone(), self.cancel.clone());
        std::thread::spawn(move || {
            let r = f(&job);
            let _ = tx.send(Event::Done(r));
        });
    }

    fn drain(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Log(s) => self.log.push(s),
                Event::Progress(p) => self.progress = p,
                Event::Done(r) => {
                    match r {
                        Ok(m) => self.log.push(format!("OK: {m}")),
                        Err(e) => self.log.push(format!("FAILED: {e}")),
                    }
                    done = true;
                }
            }
        }
        if done {
            self.rx = None;
            self.progress = 0.0;
        }
    }

    fn cloner_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Full backup and flash of the 8 MiB SPI NOR through the Ingenic bootrom (cloner mode).");
        if self.cloner_up {
            ui.colored_label(egui::Color32::LIGHT_GREEN, "Cloner device a108:c309 detected");
        } else {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                "No cloner device. Short the NOR pins and power the camera on (see the Short NOR pins tab).",
            );
        }
        ui.separator();
        let idle = !self.busy() && self.cloner_up;
        ui.horizontal(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Detect flash chip")).clicked() {
                self.start("Detect", |j| {
                    let c = cloner::Cloner::probe(j)?;
                    Ok(format!("{} ({} KiB)", c.name, c.size / 1024))
                });
            }
            if ui.add_enabled(idle, egui::Button::new("Backup flash...")).clicked() {
                if let Some(path) = rfd::FileDialog::new().set_file_name(default_backup_name()).save_file() {
                    self.start("Backup", move |j| backup(j, &path));
                }
            }
        });
        ui.separator();
        ui.heading("Flash");
        ui.horizontal(|ui| {
            ui.label(match &self.image {
                Some(p) => p.display().to_string(),
                None => "no image selected (k1cam.bin, 8 MiB)".into(),
            });
            if ui.button("Choose image...").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("flash image", &["bin"]).pick_file() {
                    self.image = Some(p);
                    self.confirm = false;
                }
            }
        });
        ui.checkbox(&mut self.backup_first, "Back up the current flash first (next to the image)");
        ui.checkbox(&mut self.verify, "Read back and verify after writing");
        ui.checkbox(&mut self.reboot_after, "Reboot the camera when done");
        ui.checkbox(&mut self.confirm, "This erases the whole flash, including U-Boot and the A/B layout");
        let can = idle && self.image.is_some() && self.confirm;
        if ui.add_enabled(can, egui::Button::new("Flash")).clicked() {
            let image = self.image.clone().unwrap();
            let (b, v, r) = (self.backup_first, self.verify, self.reboot_after);
            self.confirm = false;
            self.start("Flash", move |j| flash(j, &image, b, v, r));
        }
    }

    fn update_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("A/B update of a running k1cam camera over USB (UVC extension unit). Needs the k1cam firmware running.");
        if cfg!(windows) {
            ui.colored_label(egui::Color32::YELLOW, "Not available on Windows yet; use Linux or macOS.");
        } else if self.camera_up {
            ui.colored_label(egui::Color32::LIGHT_GREEN, "Camera 1d6b:0110 detected");
        } else {
            ui.colored_label(egui::Color32::LIGHT_RED, "No k1cam camera detected");
        }
        ui.separator();
        let idle = !self.busy() && self.camera_up && !cfg!(windows);
        ui.horizontal(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Camera info")).clicked() {
                self.start("Info", |j| {
                    let info = xu::Camera::open()?.info()?;
                    j.log(info);
                    Ok("done".into())
                });
            }
            if ui.add_enabled(idle, egui::Button::new("Dump flash...")).clicked() {
                if let Some(path) = rfd::FileDialog::new().set_file_name(default_backup_name()).save_file() {
                    let live = self.live_dump;
                    self.start("Dump", move |j| {
                        let (data, consistency) = xu::Camera::open()?.dump(j, live)?;
                        std::fs::write(&path, &data).map_err(|e| e.to_string())?;
                        Ok(format!("{} ({consistency}), sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
                    });
                }
            }
            ui.checkbox(&mut self.live_dump, "live (diagnostic, may be inconsistent)");
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(match &self.bundle {
                Some(p) => p.display().to_string(),
                None => "no bundle selected (k1cam.upd)".into(),
            });
            if ui.button("Choose bundle...").clicked() {
                self.bundle = rfd::FileDialog::new().add_filter("k1cam update", &["upd"]).pick_file();
            }
        });
        ui.checkbox(&mut self.reboot_trial, "Reboot into the new slot when staged");
        ui.horizontal(|ui| {
            if ui.add_enabled(idle && self.bundle.is_some(), egui::Button::new("Install to the inactive slot")).clicked() {
                let path = self.bundle.clone().unwrap();
                let reboot = self.reboot_trial;
                self.start("A/B update", move |j| {
                    let b = bundle::parse(std::fs::read(&path).map_err(|e| e.to_string())?)?;
                    j.log(format!("Bundle {} ({} bytes)", b.build, b.data.len()));
                    xu::Camera::open()?.upgrade(j, &b, reboot)
                });
            }
            if ui.add_enabled(idle, egui::Button::new("Reboot into trial")).clicked() {
                self.start("Reboot", |_| xu::Camera::open()?.reboot().map(|_| "rebooting".into()));
            }
            if ui.add_enabled(idle, egui::Button::new("Cancel trial")).clicked() {
                self.start("Cancel", |_| xu::Camera::open()?.cancel_trial().map(|_| "cancelled".into()));
            }
        });
    }

    fn help_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Forcing cloner (bootrom) mode");
            ui.label("The camera only talks the cloner protocol when the SoC cannot boot from the NOR. Shorting two NOR pins while it powers up does that.");
            ui.add(egui::Image::new(egui::include_image!("../docs/short-nor-pins.jpg")).max_width(ui.available_width()));
            ui.label("1. Unplug the camera's USB cable and any power.");
            ui.label("2. Connect the camera to this computer with the USB cable, but leave it unpowered if you can (or unplug it again just before step 3).");
            ui.label("3. With tweezers, hold the two pins marked in the picture together: the two right-most pins of the top-right row beside the XMC 25QH64DH NOR chip.");
            ui.label("4. While holding the short, plug in USB so the camera powers up.");
            ui.label("5. After about a second, release the short. The status at the top of the Backup & Flash tab turns green (a108:c309).");
            ui.label("If it does not show up, power-cycle and retry; the short only matters at power-up. Do not hold the short during the transfer.");
            ui.separator();
            ui.label("Linux: install the udev rule from the README. Windows: bind WinUSB to a108:c309 with Zadig. macOS: no driver needed.");
        });
    }
}

fn default_backup_name() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("k1cam-backup-{t}.bin")
}

fn backup(j: &Job, path: &std::path::Path) -> Result<String, String> {
    let data = cloner::Cloner::read_flash(j)?;
    std::fs::write(path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{} sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
}

fn flash(j: &Job, image: &std::path::Path, backup_first: bool, verify: bool, reboot: bool) -> Result<String, String> {
    let data = std::fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
    if data.len() != FLASH_SIZE {
        return Err(format!("{} must be exactly 8 MiB (got {} bytes)", image.display(), data.len()));
    }
    if backup_first {
        let mut name = image.file_stem().unwrap_or_default().to_os_string();
        name.push(format!("-backup-{}.bin", default_backup_name().trim_start_matches("k1cam-backup-").trim_end_matches(".bin")));
        let path = image.with_file_name(name);
        j.log("Backing up the current flash first");
        j.log(backup(j, &path)?);
    }
    cloner::Cloner::write_flash(j, &data, false)?;
    if verify {
        j.log("Verifying");
        let back = cloner::Cloner::read_flash(j)?;
        if back != data {
            return Err("verification failed: flash contents differ from the image".into());
        }
        j.log("Verified");
    }
    if reboot {
        cloner::Cloner::reboot(j)?;
    }
    Ok("flashed".into())
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain();
        if !self.busy() && self.last_poll.elapsed() > Duration::from_secs(1) {
            self.cloner_up = cloner::device_present();
            self.camera_up = xu::Camera::present();
            self.last_poll = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        egui::Panel::bottom("log").resizable(true).min_size(140.0).show(ui, |ui| {
            if self.busy() {
                ui.horizontal(|ui| {
                    ui.add(egui::ProgressBar::new(self.progress).show_percentage().desired_width(ui.available_width() - 80.0));
                    if ui.button("Cancel").clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for l in &self.log {
                    ui.monospace(l);
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Cloner, "Backup & Flash (cloner)");
                ui.selectable_value(&mut self.tab, Tab::Update, "A/B Update (k1cam)");
                ui.selectable_value(&mut self.tab, Tab::Help, "Short NOR pins");
            });
            ui.separator();
            match self.tab {
                Tab::Cloner => self.cloner_tab(ui),
                Tab::Update => self.update_tab(ui),
                Tab::Help => self.help_tab(ui),
            }
        });
    }
}

fn main() -> eframe::Result {
    if std::env::args().nth(1).as_deref() == Some("--info") {
        match xu::Camera::open().and_then(|mut c| c.info()) {
            Ok(i) => print!("{i}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return Ok(());
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 640.0]).with_title("k1cam flasher"),
        ..Default::default()
    };
    eframe::run_native(
        "k1cam-flasher",
        opts,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut app = App::new();
            match std::env::args().nth(1).as_deref() {
                Some("--tab=update") => app.tab = Tab::Update,
                Some("--tab=help") => app.tab = Tab::Help,
                _ => {}
            }
            Ok(Box::new(app))
        }),
    )
}
