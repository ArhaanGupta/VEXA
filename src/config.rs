//! Settings and file locations. Only preferences are saved; never audio, transcripts or answers.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Speak answers aloud (they are always shown in the on-screen card).
    pub speak_answers: bool,
    /// Speech-to-text model: "base.en-q5_1" (default, more accurate) or "tiny.en-q5_1" (fastest).
    pub stt_model: String,
    /// CPU threads for speech-to-text. 0 = automatic.
    pub stt_threads: usize,
    /// Voice-activity threshold (0-1). Raise it in noisy rooms.
    pub vad_threshold: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            speak_answers: true,
            stt_model: "base.en-q5_1".into(),
            stt_threads: 0,
            vad_threshold: 0.5,
        }
    }
}

/// Per-user data folder: %LOCALAPPDATA%\Vexa on Windows, ~/.local/share/vexa elsewhere.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Vexa");
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("vexa");
    base
}

fn config_path() -> PathBuf {
    data_dir().join("settings.json")
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(config_path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn stt_threads(&self) -> usize {
        if self.stt_threads > 0 {
            return self.stt_threads;
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // Leave room for whatever the user is doing; transcription runs in short bursts.
        (cores / 2).clamp(2, 4)
    }
}
