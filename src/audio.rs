//! Microphone capture, downmixed to mono and resampled to 16 kHz.
//!
//! Audio flows straight from the OS callback into a channel and on to voice detection. It is never
//! written to disk.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, Sample, SampleFormat};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Streaming box-filter resampler. Averaging every input sample that falls into an output slot is
/// a cheap low-pass, which is plenty for speech recognition.
pub struct Resampler {
    step: f64,
    pos: f64,
    acc: f32,
    count: u32,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self {
            step: from_hz as f64 / to_hz as f64,
            pos: 0.0,
            acc: 0.0,
            count: 0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        for &s in input {
            self.acc += s;
            self.count += 1;
            self.pos += 1.0;
            if self.pos >= self.step {
                out.push(self.acc / self.count as f32);
                self.acc = 0.0;
                self.count = 0;
                self.pos -= self.step;
            }
        }
    }
}

/// A running microphone. Mono 16 kHz samples arrive on `rx`.
pub struct Mic {
    pub rx: Receiver<Vec<f32>>,
}

/// What the UI shows about the microphone: its name and the last error, if any.
#[derive(Default)]
pub struct MicInfo {
    pub name: Mutex<String>,
    pub error: Mutex<Option<String>>,
}

impl MicInfo {
    fn set_error(&self, e: Option<String>) {
        if let Ok(mut g) = self.error.lock() {
            *g = e;
        }
    }
}

/// Start capturing from the default input device. Capture keeps running (and reconnects after a
/// device change) for the life of the process; while `paused` is set, audio is discarded in the
/// callback.
pub fn start(paused: Arc<AtomicBool>, info: Arc<MicInfo>) -> Result<Mic> {
    let (tx, rx) = channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = channel::<Result<()>>();
    thread::Builder::new()
        .name("vexa-mic".into())
        .spawn(move || {
            let mut first = Some(ready_tx);
            loop {
                let (err_tx, err_rx) = channel::<()>();
                match open_stream(tx.clone(), paused.clone(), err_tx, info.clone()) {
                    Ok(stream) => {
                        info.set_error(None);
                        if let Some(r) = first.take() {
                            let _ = r.send(Ok(()));
                        }
                        // Block until the stream reports an error (device unplugged, etc.).
                        let _ = err_rx.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        info.set_error(Some(format!("{e:#}")));
                        if let Some(r) = first.take() {
                            let _ = r.send(Err(e));
                            return;
                        }
                    }
                }
                thread::sleep(Duration::from_secs(2));
            }
        })?;
    ready_rx.recv().context("microphone thread exited")??;
    Ok(Mic { rx })
}

fn open_stream(
    tx: Sender<Vec<f32>>,
    paused: Arc<AtomicBool>,
    err_tx: Sender<()>,
    info: Arc<MicInfo>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no microphone found"))?;
    if let (Ok(d), Ok(mut name)) = (device.description(), info.name.lock()) {
        *name = d.name().to_string();
    }
    let supported = device
        .default_input_config()
        .context("microphone has no usable format")?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    // Only errors that leave the stream dead need a rebuild. Others (route changes, glitches,
    // real-time priority refused) are informational and the stream keeps running.
    let err_fn = move |e: cpal::Error| {
        if matches!(
            e.kind(),
            ErrorKind::DeviceNotAvailable
                | ErrorKind::StreamInvalidated
                | ErrorKind::HostUnavailable
                | ErrorKind::PermissionDenied
        ) {
            info.set_error(Some(e.to_string()));
            let _ = err_tx.send(());
        }
    };

    macro_rules! build {
        ($t:ty) => {{
            let mut rs = Resampler::new(rate.into(), 16_000);
            let mut mono: Vec<f32> = Vec::new();
            let tx = tx.clone();
            let paused = paused.clone();
            device.build_input_stream(
                config.clone(),
                move |data: &[$t], _: &_| {
                    if paused.load(Ordering::Relaxed) {
                        return;
                    }
                    mono.clear();
                    mono.extend(data.chunks(channels).map(|frame| {
                        frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32
                    }));
                    let mut out = Vec::with_capacity(mono.len() / 2 + 1);
                    rs.process(&mono, &mut out);
                    if !out.is_empty() {
                        let _ = tx.send(out);
                    }
                },
                err_fn,
                None,
            )?
        }};
    }

    let stream = match format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I8 => build!(i8),
        other => return Err(anyhow!("unsupported microphone sample format {other:?}")),
    };
    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48k_to_16k() {
        let mut rs = Resampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..48_000).map(|i| (i % 3) as f32).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|&v| (v - 1.0).abs() < 1e-6));
    }

    #[test]
    fn resamples_44k1_to_16k_across_chunks() {
        let mut rs = Resampler::new(44_100, 16_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            rs.process(&vec![0.5; 4_410], &mut out);
        }
        assert!(
            (out.len() as i64 - 16_000).abs() <= 1,
            "len = {}",
            out.len()
        );
    }
}
