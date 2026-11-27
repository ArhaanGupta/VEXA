// No console window on Windows release builds; Vexa lives in the tray.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod claude;
mod config;
mod engine;
mod http;
mod secrets;
mod segmenter;
mod stt;
mod wake;
mod wav;

#[cfg(windows)]
mod win;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--ask") => cli_ask(&args[1..].join(" ")),
        Some("--wav") => cli_wav(args.get(1).map(String::as_str).unwrap_or_default()),
        Some("--help") | Some("-h") => {
            println!(
                "vexa            run in the background (tray icon)\n\
                 vexa --ask TEXT ask Claude directly and print the answer\n\
                 vexa --wav FILE run a WAV file through the listening pipeline\n\
                 \nSet VEXA_DEBUG=1 to print what Vexa hears (never saved)."
            );
            Ok(())
        }
        _ => run_app(),
    }
}

#[cfg(windows)]
fn run_app() -> Result<()> {
    win::run()
}

#[cfg(not(windows))]
fn run_app() -> Result<()> {
    use engine::{Ui, UiEvent};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    struct ConsoleUi;
    impl Ui for ConsoleUi {
        fn send(&self, ev: UiEvent) {
            match ev {
                UiEvent::Status(s) => println!("{s}"),
                UiEvent::Ready => {
                    println!("Listening. Say \"Vexa\" with a request. Ctrl+C to quit.")
                }
                UiEvent::Listening => println!("(listening…)"),
                UiEvent::Working { request } => println!("> {request}"),
                UiEvent::Answer { text, .. } => println!("{text}\n"),
                UiEvent::Error(e) => eprintln!("error: {e}"),
                UiEvent::Stop => println!("(stopped)"),
            }
        }
    }
    let paused = Arc::new(AtomicBool::new(false));
    engine::start(config::Config::load(), Arc::new(ConsoleUi), paused)?;
    loop {
        std::thread::park();
    }
}

fn cli_ask(question: &str) -> Result<()> {
    let key = secrets::load_api_key().ok_or_else(|| anyhow::anyhow!("no API key configured"))?;
    let started = std::time::Instant::now();
    match claude::Claude::new(key).ask(question) {
        Ok(a) => println!("{}\n({:.1}s)", a.text, started.elapsed().as_secs_f32()),
        Err(e) => eprintln!("{e}"),
    }
    Ok(())
}

/// Offline check of the listening pipeline on a recording.
fn cli_wav(path: &str) -> Result<()> {
    use segmenter::{FRAME, Segmenter, SegmenterConfig};
    let cfg = config::Config::load();
    let audio = wav::read_16k_mono(std::path::Path::new(path))?;
    println!("{:.1}s of audio", audio.len() as f32 / 16_000.0);

    let mut detector = earshot::Detector::default();
    let mut seg = Segmenter::new(SegmenterConfig {
        threshold: cfg.vad_threshold,
        ..Default::default()
    });
    let mut utterances = Vec::new();
    let mut frame = [0f32; FRAME];
    for chunk in audio.chunks_exact(FRAME) {
        frame.copy_from_slice(chunk);
        let score = detector.predict_f32(&frame);
        if let Some(u) = seg.push(&frame, score) {
            utterances.push(u);
        }
    }
    // Flush trailing speech with synthetic silence.
    for _ in 0..200 {
        let silence = [0f32; FRAME];
        if let Some(u) = seg.push(&silence, 0.0) {
            utterances.push(u);
        }
    }
    println!("{} utterance(s)", utterances.len());

    let model = stt::ensure_model(&cfg.stt_model, |p| eprint!("\rdownloading model {p}%"))?;
    let mut stt = stt::Stt::load(&model, cfg.stt_threads())?;
    for (i, u) in utterances.iter().enumerate() {
        let t = std::time::Instant::now();
        let text = stt.transcribe(u)?;
        println!(
            "#{i} {:.1}s audio, {} ms: {text:?} -> {:?}",
            u.len() as f32 / 16_000.0,
            t.elapsed().as_millis(),
            wake::detect(&text)
        );
    }
    Ok(())
}
