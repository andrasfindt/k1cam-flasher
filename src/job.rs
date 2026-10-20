// SPDX-License-Identifier: MIT
//! Messages from worker threads to the GUI, plus cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub enum Event {
    Log(String),
    Progress(f32),
    Done(Result<String, String>),
}

pub struct Job {
    tx: Sender<Event>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn new(tx: Sender<Event>, cancel: Arc<AtomicBool>) -> Self {
        Self { tx, cancel }
    }

    pub fn log(&self, s: impl Into<String>) {
        let _ = self.tx.send(Event::Log(s.into()));
    }

    pub fn progress(&self, f: f32) {
        let _ = self.tx.send(Event::Progress(f));
    }

    pub fn check_cancel(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            Err("cancelled".into())
        } else {
            Ok(())
        }
    }
}
