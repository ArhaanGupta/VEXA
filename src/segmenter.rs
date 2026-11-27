//! Turns a stream of 16 kHz audio frames + voice-activity scores into complete utterances.
//!
//! Audio only lives here in RAM for the length of one utterance (max ~15 s). Nothing is written
//! to disk. Once an utterance is handed to speech-to-text the buffer is dropped.

use std::collections::VecDeque;

/// Samples per VAD frame (16 ms @ 16 kHz), fixed by the Earshot detector.
pub const FRAME: usize = 256;
pub const SAMPLE_RATE: usize = 16_000;

const fn frames_for_ms(ms: usize) -> usize {
    ms * SAMPLE_RATE / 1000 / FRAME
}

#[derive(Debug, Clone)]
pub struct SegmenterConfig {
    /// Score at or above which a frame counts as speech.
    pub threshold: f32,
    /// Consecutive speech frames needed to open an utterance (filters clicks and bumps).
    pub start_frames: usize,
    /// Consecutive silent frames that close an utterance.
    pub end_frames: usize,
    /// Audio kept from before speech started, so the first syllable is not clipped.
    pub preroll_frames: usize,
    /// Hard cap on utterance length.
    pub max_frames: usize,
    /// Utterances shorter than this (speech portion) are discarded.
    pub min_speech_frames: usize,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            start_frames: frames_for_ms(96),
            end_frames: frames_for_ms(560),
            preroll_frames: frames_for_ms(320),
            max_frames: frames_for_ms(15_000),
            min_speech_frames: frames_for_ms(240),
        }
    }
}

pub struct Segmenter {
    cfg: SegmenterConfig,
    preroll: VecDeque<[f32; FRAME]>,
    current: Vec<f32>,
    in_speech: bool,
    speech_run: usize,
    silence_run: usize,
    speech_frames: usize,
    frames: usize,
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Self {
        Self {
            preroll: VecDeque::with_capacity(cfg.preroll_frames + 1),
            current: Vec::new(),
            in_speech: false,
            speech_run: 0,
            silence_run: 0,
            speech_frames: 0,
            frames: 0,
            cfg,
        }
    }

    /// Whether an utterance is currently being captured.
    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// Feed one frame and its VAD score. Returns a finished utterance when one closes.
    pub fn push(&mut self, frame: &[f32; FRAME], score: f32) -> Option<Vec<f32>> {
        let speech = score >= self.cfg.threshold;

        if !self.in_speech {
            self.preroll.push_back(*frame);
            if speech {
                self.speech_run += 1;
            } else {
                self.speech_run = 0;
            }
            if self.speech_run >= self.cfg.start_frames {
                self.in_speech = true;
                self.current.clear();
                self.current.reserve(self.cfg.max_frames * FRAME);
                for f in self.preroll.drain(..) {
                    self.current.extend_from_slice(&f);
                }
                self.frames = self.current.len() / FRAME;
                self.speech_frames = self.speech_run;
                self.silence_run = 0;
            } else {
                while self.preroll.len() > self.cfg.preroll_frames + self.cfg.start_frames {
                    self.preroll.pop_front();
                }
            }
            return None;
        }

        self.current.extend_from_slice(frame);
        self.frames += 1;
        if speech {
            self.speech_frames += 1;
            self.silence_run = 0;
        } else {
            self.silence_run += 1;
        }

        if self.silence_run >= self.cfg.end_frames || self.frames >= self.cfg.max_frames {
            return self.finish();
        }
        None
    }

    fn finish(&mut self) -> Option<Vec<f32>> {
        self.in_speech = false;
        self.speech_run = 0;
        self.silence_run = 0;
        self.frames = 0;
        let enough = self.speech_frames >= self.cfg.min_speech_frames;
        self.speech_frames = 0;
        let audio = std::mem::take(&mut self.current);
        if enough { Some(audio) } else { None }
    }

    /// Drop any partial utterance (e.g. when listening is paused).
    pub fn reset(&mut self) {
        self.preroll.clear();
        self.current = Vec::new();
        self.in_speech = false;
        self.speech_run = 0;
        self.silence_run = 0;
        self.speech_frames = 0;
        self.frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(seg: &mut Segmenter, scores: &[f32]) -> Vec<Vec<f32>> {
        let frame = [0.1f32; FRAME];
        scores.iter().filter_map(|&s| seg.push(&frame, s)).collect()
    }

    fn pattern(parts: &[(f32, usize)]) -> Vec<f32> {
        parts
            .iter()
            .flat_map(|&(s, n)| std::iter::repeat_n(s, n))
            .collect()
    }

    #[test]
    fn emits_one_utterance_after_trailing_silence() {
        let cfg = SegmenterConfig::default();
        let mut seg = Segmenter::new(cfg.clone());
        let scores = pattern(&[(0.0, 50), (0.9, 100), (0.0, cfg.end_frames + 5)]);
        let out = run(&mut seg, &scores);
        assert_eq!(out.len(), 1);
        let frames = out[0].len() / FRAME;
        // preroll + speech + trailing silence
        assert!(frames >= 100 + cfg.end_frames, "frames = {frames}");
        assert!(frames <= 100 + cfg.end_frames + cfg.preroll_frames + cfg.start_frames);
    }

    #[test]
    fn ignores_short_blips() {
        let mut seg = Segmenter::new(SegmenterConfig::default());
        let scores = pattern(&[(0.9, 3), (0.0, 100), (0.9, 4), (0.0, 100)]);
        assert!(run(&mut seg, &scores).is_empty());
    }

    #[test]
    fn short_pauses_do_not_split_an_utterance() {
        let cfg = SegmenterConfig::default();
        let mut seg = Segmenter::new(cfg.clone());
        let scores = pattern(&[
            (0.9, 60),
            (0.0, cfg.end_frames / 2),
            (0.9, 60),
            (0.0, cfg.end_frames + 1),
        ]);
        assert_eq!(run(&mut seg, &scores).len(), 1);
    }

    #[test]
    fn caps_long_utterances() {
        let cfg = SegmenterConfig::default();
        let mut seg = Segmenter::new(cfg.clone());
        let scores = pattern(&[(0.9, cfg.max_frames * 2 + 10)]);
        let out = run(&mut seg, &scores);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|u| u.len() / FRAME <= cfg.max_frames));
    }
}
