//! Local speech-to-text with whisper.cpp. Runs only on utterances the voice detector found, so the
//! CPU is idle while nobody is talking.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn model_path(name: &str) -> PathBuf {
    crate::config::data_dir()
        .join("models")
        .join(format!("ggml-{name}.bin"))
}

fn model_url(name: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin")
}

/// Download the model if it is not present yet. `progress` receives (bytes done, total bytes or 0).
pub fn ensure_model(name: &str, mut progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    let path = model_path(name);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("part");
    let mut resp = crate::http::agent(std::time::Duration::from_secs(1800))
        .get(&model_url(name))
        .call()
        .context("could not download the speech model")?;
    if resp.status() != 200 {
        bail!(
            "could not download the speech model (HTTP {})",
            resp.status()
        );
    }
    let total: u64 = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut reader = resp.body_mut().with_config().limit(1 << 30).reader();
    let mut file = std::fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let mut last_report: u64 = 0;
    progress(0, total);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        if done - last_report >= 2 * 1024 * 1024 {
            last_report = done;
            progress(done, total);
        }
    }
    file.flush()?;
    drop(file);
    if done < 10 * 1024 * 1024 || (total > 0 && done != total) {
        let _ = std::fs::remove_file(&tmp);
        bail!("speech model download was incomplete");
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Spelling and style hints for speech recognition.
#[cfg(feature = "stt")]
const STT_PROMPT: &str = "Hey Vexa, what's the weather in Sydney? Why was that, Vexa? \
Vexa, what did Nvidia, Apple, Microsoft, Tesla, Amazon, Google and AMD close at on Friday?";

#[cfg(feature = "stt")]
pub struct Stt {
    state: whisper_rs::WhisperState,
    threads: i32,
}

#[cfg(feature = "stt")]
impl Stt {
    pub fn load(path: &Path, threads: usize) -> Result<Self> {
        use whisper_rs::{WhisperContext, WhisperContextParameters};
        // Route whisper.cpp's console output into the (silent) log facade.
        whisper_rs::install_logging_hooks();
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .context("could not load the speech model")?;
        let state = ctx
            .create_state()
            .context("could not start speech recognition")?;
        Ok(Self {
            state,
            threads: threads as i32,
        })
    }

    /// Transcribe one utterance of 16 kHz mono audio.
    pub fn transcribe(&mut self, audio: &[f32]) -> Result<String> {
        use whisper_rs::{FullParams, SamplingStrategy};
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some("en"));
        p.set_n_threads(self.threads);
        p.set_no_context(true);
        p.set_single_segment(true);
        p.set_no_timestamps(true);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        // Example text in the style we expect biases spelling towards the wake word and the names
        // people ask about most, which the small model otherwise mangles ("Envady", "mixer").
        p.set_initial_prompt(STT_PROMPT);
        // whisper normally encodes a full 30 s window. Encoding only what we have (plus margin)
        // makes short commands several times faster on CPU.
        let secs = audio.len() as f32 / 16_000.0;
        let ctx = ((secs * 50.0) as i32 + 128).min(1500);
        p.set_audio_ctx(ctx);

        // whisper needs at least ~1 s of audio; pad short clips with silence.
        let mut padded;
        let input = if audio.len() < 16_000 + 1_600 {
            padded = audio.to_vec();
            padded.resize(16_000 + 1_600, 0.0);
            &padded[..]
        } else {
            audio
        };

        self.state
            .full(p, input)
            .context("speech recognition failed")?;
        let mut text = String::new();
        for seg in self.state.as_iter() {
            if seg.no_speech_probability() > 0.8 {
                continue;
            }
            text.push_str(&seg.to_str_lossy()?);
        }
        Ok(text.trim().to_string())
    }
}

/// Stand-in used when the crate is built without the `stt` feature (cross-compile checks only).
#[cfg(not(feature = "stt"))]
pub struct Stt;

#[cfg(not(feature = "stt"))]
impl Stt {
    pub fn load(_path: &Path, _threads: usize) -> Result<Self> {
        bail!("built without speech-to-text support")
    }
    pub fn transcribe(&mut self, _audio: &[f32]) -> Result<String> {
        bail!("built without speech-to-text support")
    }
}
