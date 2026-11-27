//! Tiny WAV reader for the `--wav` test mode (16-bit PCM or 32-bit float).

use anyhow::{Result, bail};
use std::path::Path;

pub fn read_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let data = std::fs::read(path)?;
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let (mut channels, mut rate, mut bits, mut format) = (0u16, 0u32, 0u16, 0u16);
    let mut samples: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= data.len() {
        let id = &data[i..i + 4];
        let len = u32::from_le_bytes(data[i + 4..i + 8].try_into()?) as usize;
        let body = &data[i + 8..(i + 8 + len).min(data.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                format = u16::from_le_bytes([body[0], body[1]]);
                channels = u16::from_le_bytes([body[2], body[3]]);
                rate = u32::from_le_bytes(body[4..8].try_into()?);
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => samples = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1);
    }
    let Some(raw) = samples else {
        bail!("WAV has no data chunk")
    };
    if channels == 0 {
        bail!("WAV has no format chunk");
    }
    let interleaved: Vec<f32> = match (format, bits) {
        (1, 16) => raw
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect(),
        (3, 32) => raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        _ => bail!("unsupported WAV encoding (use 16-bit PCM or 32-bit float)"),
    };
    let ch = channels as usize;
    let mono: Vec<f32> = interleaved
        .chunks_exact(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    let mut out = Vec::with_capacity(mono.len());
    crate::audio::Resampler::new(rate, 16_000).process(&mono, &mut out);
    Ok(out)
}
