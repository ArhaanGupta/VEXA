//! Spoken answers through the built-in Windows speech engine (SAPI). Local and free.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Media::Speech::{
    ISpVoice, SPF_ASYNC, SPF_IS_NOT_XML, SPF_PURGEBEFORESPEAK, SPVOICESTATUS, SpVoice,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::PCWSTR;

enum Cmd {
    Speak(String),
    Stop,
}

pub struct Tts {
    tx: Sender<Cmd>,
}

/// SPRS_IS_SPEAKING from sapi.h.
const SPRS_IS_SPEAKING: u32 = 2;

impl Tts {
    /// `on_done` is called (from the speech thread) when an answer finishes or is stopped.
    pub fn start(on_done: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let _ = thread::Builder::new()
            .name("vexa-tts".into())
            .spawn(move || worker(rx, on_done));
        Self { tx }
    }

    pub fn speak(&self, text: &str) {
        let _ = self.tx.send(Cmd::Speak(text.to_string()));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

fn worker(rx: Receiver<Cmd>, on_done: impl Fn()) {
    let voice: ISpVoice = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match CoCreateInstance(&SpVoice, None, CLSCTX_ALL) {
            Ok(v) => v,
            Err(_) => {
                // No speech engine: drain commands and report each as finished immediately.
                while let Ok(cmd) = rx.recv() {
                    if let Cmd::Speak(_) = cmd {
                        on_done();
                    }
                }
                return;
            }
        }
    };
    unsafe {
        let _ = voice.SetRate(1);
    }
    let mut speaking = false;
    let mut started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(Cmd::Speak(text)) => {
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let flags = (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0 | SPF_IS_NOT_XML.0) as u32;
                speaking = unsafe { voice.Speak(PCWSTR(wide.as_ptr()), flags, None) }.is_ok();
                started = Instant::now();
                if !speaking {
                    on_done();
                }
            }
            Ok(Cmd::Stop) => {
                unsafe {
                    let empty = [0u16];
                    let _ = voice.Speak(
                        PCWSTR(empty.as_ptr()),
                        (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0) as u32,
                        None,
                    );
                }
                if speaking {
                    speaking = false;
                    on_done();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Give the engine a moment to start before treating "not speaking" as finished.
        if speaking && started.elapsed() > Duration::from_millis(500) {
            let mut status = SPVOICESTATUS::default();
            let running = unsafe { voice.GetStatus(&mut status, std::ptr::null_mut()) }
                .map(|_| status.dwRunningState == SPRS_IS_SPEAKING)
                .unwrap_or(false);
            if !running {
                speaking = false;
                on_done();
            }
        }
    }
}
