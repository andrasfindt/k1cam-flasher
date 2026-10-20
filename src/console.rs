// SPDX-License-Identifier: MIT
//! The camera's console on its USB serial port (CDC-ACM, found by the k1cam USB
//! ids). It is opened as soon as it appears, unless another program has it, and
//! shown as a line terminal: type a line, Enter sends it.

use eframe::egui;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_LINES: usize = 5000;

/// Where the parser is in an escape sequence.
enum Seq {
    Text,
    Escape,
    Csi(Vec<u8>),
    Osc,
    OscEscape,
}

/// What a dumb terminal would show: lines of text, with carriage return,
/// backspace, tab and line erase applied, and other escape sequences (colours,
/// cursor queries) dropped.
pub struct Term {
    lines: VecDeque<String>,
    cur: Vec<char>,
    col: usize,
    seq: Seq,
    utf8: Vec<u8>,
}

impl Term {
    pub fn new() -> Self {
        Self { lines: VecDeque::new(), cur: Vec::new(), col: 0, seq: Seq::Text, utf8: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.cur.clear();
        self.col = 0;
    }

    /// Lines so far, the unfinished one last.
    pub fn rows(&self) -> usize {
        self.lines.len() + 1
    }

    pub fn row(&self, i: usize) -> String {
        match self.lines.get(i) {
            Some(l) => l.clone(),
            None => self.cur.iter().collect(),
        }
    }

    pub fn text(&self) -> String {
        let mut s: String = self.lines.iter().flat_map(|l| [l.as_str(), "\n"]).collect();
        s.extend(self.cur.iter());
        s
    }

    fn put(&mut self, c: char) {
        if self.col < self.cur.len() {
            self.cur[self.col] = c;
        } else {
            self.cur.resize(self.col, ' ');
            self.cur.push(c);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.lines.push_back(self.cur.drain(..).collect());
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        self.col = 0;
    }

    fn csi(&mut self, fin: u8, params: &[u8]) {
        let n = std::str::from_utf8(params).ok().and_then(|p| p.split(';').next()?.parse::<usize>().ok());
        match fin {
            b'K' => match n.unwrap_or(0) {
                0 => self.cur.truncate(self.col),
                1 => self.cur.iter_mut().take(self.col).for_each(|c| *c = ' '),
                _ => self.cur.clear(),
            },
            b'C' => self.col += n.unwrap_or(1).max(1),
            b'D' => self.col = self.col.saturating_sub(n.unwrap_or(1).max(1)),
            b'G' => self.col = n.unwrap_or(1).saturating_sub(1),
            b'J' if n == Some(2) || n == Some(3) => self.clear(),
            _ => {}
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match &mut self.seq {
                Seq::Text => {}
                Seq::Escape => {
                    self.seq = match b {
                        b'[' => Seq::Csi(Vec::new()),
                        b']' => Seq::Osc,
                        _ => Seq::Text,
                    };
                    continue;
                }
                Seq::Csi(p) => {
                    if (0x40..=0x7e).contains(&b) {
                        let params = std::mem::take(p);
                        self.seq = Seq::Text;
                        self.csi(b, &params);
                    } else if p.len() < 32 {
                        p.push(b);
                    }
                    continue;
                }
                Seq::Osc => {
                    match b {
                        0x07 => self.seq = Seq::Text,
                        0x1b => self.seq = Seq::OscEscape,
                        _ => {}
                    }
                    continue;
                }
                Seq::OscEscape => {
                    self.seq = if b == b'\\' { Seq::Text } else { Seq::Osc };
                    continue;
                }
            }
            if b >= 0x80 || !self.utf8.is_empty() {
                self.utf8.push(b);
                match std::str::from_utf8(&self.utf8) {
                    Ok(s) => {
                        let c = s.chars().next().unwrap_or('\u{fffd}');
                        self.utf8.clear();
                        self.put(c);
                    }
                    Err(e) if e.error_len().is_none() && self.utf8.len() < 4 => {}
                    Err(_) => {
                        self.utf8.clear();
                        self.put('\u{fffd}');
                    }
                }
                continue;
            }
            match b {
                0x1b => self.seq = Seq::Escape,
                b'\r' => self.col = 0,
                b'\n' => self.newline(),
                0x08 => self.col = self.col.saturating_sub(1),
                b'\t' => {
                    for _ in 0..8 - self.col % 8 {
                        self.put(' ');
                    }
                }
                0x20..=0x7e => self.put(b as char),
                _ => {}
            }
        }
    }
}

struct Conn {
    name: String,
    port: Box<dyn serialport::SerialPort>,
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct Console {
    /// The camera's console port has been seen: the tab exists from then on.
    seen: bool,
    port: Option<String>,
    conn: Option<Conn>,
    /// Connect on its own; off once the user disconnects.
    auto: bool,
    /// Auto-connect was tried for this appearance of the port.
    tried: bool,
    appeared: Instant,
    note: String,
    term: Term,
    input: String,
    history: Vec<String>,
    browse: Option<usize>,
    scanned: Option<Instant>,
}

/// The serial port of an attached k1cam camera, if any.
fn find_port() -> Option<String> {
    serialport::available_ports().ok()?.into_iter().find_map(|p| match p.port_type {
        serialport::SerialPortType::UsbPort(u) if crate::xu::CAMERA_IDS.contains(&(u.vid, u.pid)) => Some(p.port_name),
        _ => None,
    })
}

impl Console {
    pub fn new() -> Self {
        Self {
            seen: false,
            port: None,
            conn: None,
            auto: true,
            tried: false,
            appeared: Instant::now(),
            note: String::new(),
            term: Term::new(),
            input: String::new(),
            history: Vec::new(),
            browse: None,
            scanned: None,
        }
    }

    pub fn seen(&self) -> bool {
        self.seen
    }

    /// Takes in what arrived, and once a second looks for the port and connects
    /// when it should.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.conn {
            let mut closed = false;
            loop {
                match c.rx.try_recv() {
                    Ok(b) => self.term.feed(&b),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                self.note = format!("{} closed; reconnecting when the camera's console is back", c.name);
                self.conn = None;
                self.tried = false;
            }
        }
        if self.scanned.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.scanned = Some(Instant::now());
        let found = find_port();
        if found != self.port {
            self.tried = false;
            self.appeared = Instant::now();
            if self.conn.as_ref().is_some_and(|c| Some(&c.name) != found.as_ref()) {
                self.conn = None;
            }
            if found.is_none() && self.seen {
                self.note = "Waiting for the camera's console".into();
            }
            self.port = found;
        }
        if let Some(name) = self.port.clone() {
            self.seen = true;
            if self.auto && !self.tried && self.conn.is_none() {
                // Programs that probe new serial ports (ModemManager) hold them
                // for a moment: keep trying for a while after it appears.
                let in_use = self.connect(&name, false, ctx);
                self.tried = !in_use || self.appeared.elapsed() > Duration::from_secs(10);
            }
        }
    }

    /// Opens the port (unless, without `force`, another program has it: then
    /// returns true).
    fn connect(&mut self, name: &str, force: bool, ctx: &egui::Context) -> bool {
        #[cfg(target_os = "linux")]
        if !force {
            let users = crate::linux::tty_users(name);
            if !users.is_empty() {
                self.note = format!("{name} is open in {}; not connecting so that it keeps its output", users.join(", "));
                return true;
            }
        }
        let _ = force;
        let opened = serialport::new(name, 115200).timeout(Duration::from_millis(100)).open().and_then(|p| Ok((p.try_clone()?, p)));
        let (mut reader, port) = match opened {
            Ok(pair) => pair,
            Err(e) => {
                self.note = format!("{name}: {e}");
                return false;
            }
        };
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, ctx) = (stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
                    Err(_) => break,
                }
            }
            ctx.request_repaint();
        });
        self.note = format!("Connected to {name}. Other programs can't open it until you disconnect.");
        self.conn = Some(Conn { name: name.into(), port, rx, stop });
        false
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Some(c) = &mut self.conn {
            if let Err(e) = c.port.write_all(bytes) {
                self.note = format!("{}: {e}", c.name);
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let connected = self.conn.is_some();
        ui.horizontal(|ui| {
            if connected {
                if ui.button("Disconnect").clicked() {
                    self.auto = false;
                    self.conn = None;
                    self.note = "Disconnected".into();
                }
            } else if let Some(name) = self.port.clone() {
                if ui.button("Connect").clicked() {
                    self.auto = true;
                    self.tried = true;
                    self.connect(&name, true, ui.ctx());
                }
            }
            if ui.add_enabled(connected, egui::Button::new("Ctrl-C")).on_hover_text("Stops what runs in the foreground, e.g. logf").clicked() {
                self.send(&[3]);
            }
            if ui.button("Clear").clicked() {
                self.term.clear();
            }
            if ui.button("Copy all").clicked() {
                ui.ctx().copy_text(self.term.text());
            }
        });
        ui.label(&self.note);
        let row = ui.text_style_height(&egui::TextStyle::Monospace);
        let input = ui.spacing().interact_size.y + ui.spacing().item_spacing.y * 2.0;
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height((ui.available_height() - input).max(row))
            .show_rows(ui, row, self.term.rows(), |ui, rows| {
                for i in rows {
                    ui.add(egui::Label::new(egui::RichText::new(self.term.row(i)).monospace()).wrap_mode(egui::TextWrapMode::Extend));
                }
            });
        let edit = egui::TextEdit::singleline(&mut self.input)
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .hint_text("Enter sends the line; an empty one wakes the console's prompt");
        let r = ui.add_enabled(connected, edit);
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let line = std::mem::take(&mut self.input);
            self.send(format!("{line}\r").as_bytes());
            if !line.is_empty() && self.history.last() != Some(&line) {
                self.history.push(line);
            }
            self.browse = None;
            r.request_focus();
        } else if r.has_focus() {
            let (up, down) = ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown)));
            if up && !self.history.is_empty() {
                let i = self.browse.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.browse = Some(i);
                self.input = self.history[i].clone();
            } else if down {
                match self.browse {
                    Some(i) if i + 1 < self.history.len() => {
                        self.browse = Some(i + 1);
                        self.input = self.history[i + 1].clone();
                    }
                    Some(_) => {
                        self.browse = None;
                        self.input.clear();
                    }
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(chunks: &[&[u8]]) -> String {
        let mut t = Term::new();
        chunks.iter().for_each(|c| t.feed(c));
        t.text()
    }

    #[test]
    fn line_discipline() {
        assert_eq!(shown(&[b"a\r\nb\r\n"]), "a\nb\n");
        // The shell's echo of a backspace, and line redraws.
        assert_eq!(shown(&[b"lsx\x08 \x08\r\n"]), "ls \n");
        assert_eq!(shown(&[b"abcdef\r\x1b[Kxy"]), "xy");
        assert_eq!(shown(&[b"abcdef\x1b[3D\x1b[K!"]), "abc!");
        assert_eq!(shown(&[b"abc\rX"]), "Xbc");
        assert_eq!(shown(&[b"a\tb"]), "a       b");
        // Colours, cursor queries and titles leave nothing behind.
        assert_eq!(shown(&[b"\x1b[1;34m~ #\x1b[0m \x1b[6n\x1b]0;cam\x07$"]), "~ # $");
        // Sequences and UTF-8 split across reads.
        assert_eq!(shown(&[b"\x1b[", b"1;3", b"1mok\xc3", b"\xa9"]), "ok\u{e9}");
        assert_eq!(shown(&[b"\xff!"]), "\u{fffd}!");
        // `clear`
        assert_eq!(shown(&[b"old\r\n\x1b[H\x1b[2Jnew"]), "new");
    }

    #[test]
    fn keeps_the_last_lines() {
        let mut t = Term::new();
        for i in 0..MAX_LINES + 10 {
            t.feed(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(t.rows(), MAX_LINES + 1);
        assert_eq!(t.row(0), "10");
    }
}
