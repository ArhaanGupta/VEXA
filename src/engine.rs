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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    Answer {
        request: String,
        text: String,
        /// Tokens, searches and estimated cost of this answer.
        usage: String,
    },
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
    pub mic: Arc<crate::audio::MicInfo>,
    /// When audio last arrived (ms since the Unix epoch).
    last_audio_ms: AtomicU64,
    /// The mic has delivered exact digital silence for a while (typically blocked by privacy settings).
    silent: AtomicBool,
    /// An answer is being read aloud; what the mic hears is probably Vexa itself.
    speaking: AtomicBool,
    /// Until when an utterance without the wake word counts as a follow-up (ms since epoch).
    follow_up_until_ms: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Stats {
    /// Smoothed microphone level, 0.0-1.0.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Mark whether an answer is currently being read aloud.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_speaking(&self, on: bool) {
        self.speaking.store(on, Ordering::Relaxed);
    }

    /// Accept one follow-up question without the wake word for the next `secs` seconds.
    pub fn open_follow_up(&self, secs: u64) {
        self.follow_up_until_ms
            .store(now_ms() + secs * 1000, Ordering::Relaxed);
    }

    fn close_follow_up(&self) {
        self.follow_up_until_ms.store(0, Ordering::Relaxed);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn follow_up_open(&self) -> bool {
        now_ms() < self.follow_up_until_ms.load(Ordering::Relaxed)
    }

    /// Name of the microphone in use.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn mic_name(&self) -> String {
        self.mic.name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// A plain-language description of a microphone problem, if there is one.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn mic_problem(&self) -> Option<String> {
        if !self.ready.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(e) = self.mic.error.lock().ok().and_then(|e| e.clone()) {
            return Some(format!(
                "Microphone error: {e}. Check it is plugged in and enabled."
            ));
        }
        if now_ms().saturating_sub(self.last_audio_ms.load(Ordering::Relaxed)) > 3_000 {
            return Some("No audio is arriving from the microphone.".into());
        }
        if self.silent.load(Ordering::Relaxed) {
            return Some(
                "The microphone is sending pure silence. Windows may be blocking it: Settings > \
                 Privacy & security > Microphone > turn on \"Let desktop apps access your microphone\". \
                 Also check the mic isn't muted."
                    .into(),
            );
        }
        None
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
    let mic = crate::audio::start(paused.clone(), stats.mic.clone())?;
    stats.last_audio_ms.store(now_ms(), Ordering::Relaxed);

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
    let mut silent_frames = 0usize;
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
        stats.last_audio_ms.store(now_ms(), Ordering::Relaxed);
        pending.extend_from_slice(&chunk);
        let mut used = 0;
        while pending.len() - used >= FRAME {
            for (dst, src) in frame.iter_mut().zip(&pending[used..used + FRAME]) {
                *dst = src.clamp(-1.0, 1.0);
            }
            used += FRAME;
            // Level meter: fast attack, slow release, perceptual (square-root) scale.
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
            // A real mic always has some noise; exact zeros for 2 s means it is blocked or muted.
            silent_frames = if rms < 1e-6 { silent_frames + 1 } else { 0 };
            stats.silent.store(silent_frames > 125, Ordering::Relaxed);
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
            // Without the wake word, only accept speech right after "Hey Vexa" or an answer,
            // and never while an answer is being read aloud (that is Vexa hearing itself).
            Wake::None if stats.speaking.load(Ordering::Relaxed) => None,
            Wake::None => {
                let armed = armed_until
                    .take()
                    .is_some_and(|until| Instant::now() < until);
                let follow_up = stats.follow_up_open() && wake::is_follow_up(&text);
                (armed || follow_up)
                    .then(|| wake::clean_transcript(&text))
                    .filter(|t| !t.is_empty())
            }
        };
        let Some(request) = request else { continue };
        armed_until = None;
        stats.close_follow_up();

        if wake::control(&request) == Some(Control::Stop) {
            ui.send(UiEvent::Stop);
            continue;
        }
        dispatch(request, ui.clone());
    }
}

/// The current conversation, kept in memory only.
static CONVERSATION: std::sync::Mutex<crate::claude::Conversation> =
    std::sync::Mutex::new(crate::claude::Conversation::new());

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
            let history = CONVERSATION
                .lock()
                .map(|mut c| c.history(started))
                .unwrap_or_default();
            match Claude::new(key).ask(&request, &history) {
                Ok(a) => {
                    debug!(
                        "answered in {} ms ({})",
                        started.elapsed().as_millis(),
                        a.usage.summary()
                    );
                    if let Ok(mut c) = CONVERSATION.lock() {
                        c.record(request.clone(), a.text.clone(), Instant::now());
                    }
                    ui.send(UiEvent::Answer {
                        request,
                        text: a.text,
                        usage: a.usage.summary(),
                    });
                }
                Err(e) => ui.send(UiEvent::Error(e.to_string())),
            }
        });
}
