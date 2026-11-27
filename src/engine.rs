//! The listening pipeline: mic -> voice detection -> utterances -> speech-to-text -> wake word ->
//! Claude (in the background) -> UI. Nothing in here touches the disk except the one-time model
//! download.

use crate::claude::Claude;
use crate::config::Config;
use crate::segmenter::{FRAME, Segmenter, SegmenterConfig};
use crate::stt::Stt;
use crate::wake::{self, Control, Wake};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

/// How long after a bare "Hey Vexa" the next utterance counts as the request.
const ARMED_WINDOW: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum UiEvent {
    /// One-off status line (model download, errors at start-up).
    Status(String),
    /// Wake word heard on its own; waiting for the request.
    Listening,
    /// A request was captured and is being worked on in the background.
    Working { request: String },
    /// The answer is ready.
    Answer { request: String, text: String },
    /// Something failed; shown briefly.
    Error(String),
    /// Stop speaking and hide the card.
    Stop,
    /// The pipeline is up and listening.
    Ready,
    /// Text recognised from one utterance (shown on screen only, never stored).
    Heard { text: String, addressed: bool },
}

/// Live pipeline state the UI can poll each frame without extra messages.
#[derive(Default)]
pub struct Stats {
    level: AtomicU32,
    pub in_speech: AtomicBool,
    pub transcribing: AtomicBool,
    pub ready: AtomicBool,
}

impl Stats {
    /// Smoothed microphone level, 0.0-1.0.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
    }
}

pub trait Ui: Send + Sync + 'static {
    fn send(&self, ev: UiEvent);
}

pub fn debug_enabled() -> bool {
    std::env::var_os("VEXA_DEBUG").is_some()
}

macro_rules! debug {
    ($($t:tt)*) => { if debug_enabled() { eprintln!("[vexa] {}", format!($($t)*)); } };
}

/// Start the pipeline on background threads. Returns once the microphone is open.
pub fn start(
    cfg: Config,
    ui: Arc<dyn Ui>,
    paused: Arc<AtomicBool>,
    stats: Arc<Stats>,
) -> Result<()> {
    let ui2 = ui.clone();
    let model = crate::stt::ensure_model(&cfg.stt_model, |done, total| {
        let mb = |b: u64| b / (1024 * 1024);
        let msg = if total > 0 {
            format!(
                "Downloading speech model (one time only): {} of {} MB",
                mb(done),
                mb(total)
            )
        } else {
            format!("Downloading speech model (one time only): {} MB", mb(done))
        };
        ui2.send(UiEvent::Status(msg));
    })?;
    ui.send(UiEvent::Status("Loading speech model…".into()));
    let stt = Stt::load(&model, cfg.stt_threads())?;

    let (utt_tx, utt_rx) = sync_channel::<Vec<f32>>(2);
    let mic = crate::audio::start(paused.clone())?;

    let vad_cfg = SegmenterConfig {
        threshold: cfg.vad_threshold,
        ..Default::default()
    };
    let vad_stats = stats.clone();
    thread::Builder::new()
        .name("vexa-vad".into())
        .spawn(move || vad_loop(mic.rx, utt_tx, paused, vad_cfg, vad_stats))?;
    let ui3 = ui.clone();
    let stt_stats = stats.clone();
    thread::Builder::new()
        .name("vexa-stt".into())
        .spawn(move || stt_loop(stt, utt_rx, ui3, stt_stats))?;
    stats.ready.store(true, Ordering::Relaxed);
    ui.send(UiEvent::Ready);
    Ok(())
}

fn vad_loop(
    rx: Receiver<Vec<f32>>,
    tx: SyncSender<Vec<f32>>,
    paused: Arc<AtomicBool>,
    cfg: SegmenterConfig,
    stats: Arc<Stats>,
) {
    let mut level = 0f32;
    let mut detector = earshot::Detector::default();
    let mut seg = Segmenter::new(cfg);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 8);
    let mut frame = [0f32; FRAME];
    while let Ok(chunk) = rx.recv() {
        if paused.load(Ordering::Relaxed) {
            seg.reset();
            detector.reset();
            pending.clear();
            stats.set_level(0.0);
            stats.in_speech.store(false, Ordering::Relaxed);
            continue;
        }
        pending.extend_from_slice(&chunk);
        let mut used = 0;
        while pending.len() - used >= FRAME {
            for (dst, src) in frame.iter_mut().zip(&pending[used..used + FRAME]) {
                *dst = src.clamp(-1.0, 1.0);
            }
            used += FRAME;
            // Level meter: fast attack, slow release, perceptual (square-root) scale.
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
            let target = (rms * 10.0).sqrt().min(1.0);
            level = if target > level {
                target
            } else {
                level * 0.9 + target * 0.1
            };
            stats.set_level(level);
            let score = detector.predict_f32(&frame);
            let utterance = seg.push(&frame, score);
            stats.in_speech.store(seg.in_speech(), Ordering::Relaxed);
            if let Some(utterance) = utterance {
                match tx.try_send(utterance) {
                    Ok(()) => {}
                    // Speech-to-text is still busy with earlier speech: drop rather than lag behind.
                    Err(TrySendError::Full(_)) => debug!("stt busy, dropped an utterance"),
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
        }
        pending.drain(..used);
    }
}

fn stt_loop(mut stt: Stt, rx: Receiver<Vec<f32>>, ui: Arc<dyn Ui>, stats: Arc<Stats>) {
    let mut armed_until: Option<Instant> = None;
    while let Ok(audio) = rx.recv() {
        let started = Instant::now();
        stats.transcribing.store(true, Ordering::Relaxed);
        let result = stt.transcribe(&audio);
        stats.transcribing.store(false, Ordering::Relaxed);
        let text = match result {
            Ok(t) => t,
            Err(e) => {
                debug!("stt error: {e}");
                continue;
            }
        };
        drop(audio);
        debug!("heard ({} ms): {text:?}", started.elapsed().as_millis());

        let wake = wake::detect(&text);
        let cleaned = wake::clean_transcript(&text);
        if !cleaned.is_empty() {
            ui.send(UiEvent::Heard {
                text: cleaned,
                addressed: wake != Wake::None,
            });
        }

        let request = match wake {
            Wake::Request(r) => Some(r),
            Wake::WakeOnly => {
                armed_until = Some(Instant::now() + ARMED_WINDOW);
                ui.send(UiEvent::Listening);
                None
            }
            Wake::None => match armed_until.take() {
                Some(until) if Instant::now() < until => {
                    let t = wake::clean_transcript(&text);
                    (!t.is_empty()).then_some(t)
                }
                _ => None,
            },
        };
        let Some(request) = request else { continue };
        armed_until = None;

        if wake::control(&request) == Some(Control::Stop) {
            ui.send(UiEvent::Stop);
            continue;
        }
        dispatch(request, ui.clone());
    }
}

/// Run a request on its own thread so listening never waits on the network.
pub fn dispatch(request: String, ui: Arc<dyn Ui>) {
    let _ = thread::Builder::new()
        .name("vexa-task".into())
        .spawn(move || {
            ui.send(UiEvent::Working {
                request: request.clone(),
            });
            let Some(key) = crate::secrets::load_api_key() else {
                ui.send(UiEvent::Error(
                    "Add your Anthropic API key in Vexa settings (tray icon).".into(),
                ));
                return;
            };
            let started = Instant::now();
            match Claude::new(key).ask(&request) {
                Ok(a) => {
                    debug!("answered in {} ms", started.elapsed().as_millis());
                    ui.send(UiEvent::Answer {
                        request,
                        text: a.text,
                    });
                }
                Err(e) => ui.send(UiEvent::Error(e.to_string())),
            }
        });
}
