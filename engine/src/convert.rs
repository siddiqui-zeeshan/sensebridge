//! 48 kHz audio to the controller's 3 kHz signed 8-bit haptics format.

pub const CHANNELS: usize = 2;
pub type Frame = [f32; CHANNELS];

/// Streaming low-pass filter and decimation, 48 kHz frames to 3 kHz.
pub struct Decimator {
    taps: Vec<f32>,
    ratio: usize,
    buf: Vec<Frame>,
}

impl Default for Decimator {
    fn default() -> Self {
        Self::new(16, 127, 1100.0, 48000.0)
    }
}

impl Decimator {
    pub fn new(ratio: usize, ntaps: usize, cutoff: f64, rate: f64) -> Self {
        // Windowed-sinc low-pass (Hamming), normalised to unity gain at DC.
        let centre = (ntaps as f64 - 1.0) / 2.0;
        let mut taps: Vec<f64> = (0..ntaps)
            .map(|k| {
                let x = 2.0 * cutoff / rate * (k as f64 - centre);
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let window = 0.54
                    - 0.46 * (2.0 * std::f64::consts::PI * k as f64 / (ntaps as f64 - 1.0)).cos();
                sinc * window
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self {
            taps: taps.into_iter().map(|t| t as f32).collect(),
            ratio,
            buf: vec![[0.0; CHANNELS]; ntaps - 1],
        }
    }

    /// Feed any number of input frames; returns the output frames they complete.
    pub fn process(&mut self, frames: &[Frame]) -> Vec<Frame> {
        self.buf.extend_from_slice(frames);
        let ntaps = self.taps.len();
        if self.buf.len() < ntaps {
            return Vec::new();
        }
        let count = (self.buf.len() - ntaps) / self.ratio + 1;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let window = &self.buf[i * self.ratio..i * self.ratio + ntaps];
            let mut acc = [0.0f32; CHANNELS];
            for (frame, tap) in window.iter().zip(&self.taps) {
                for c in 0..CHANNELS {
                    acc[c] += frame[c] * tap;
                }
            }
            out.push(acc);
        }
        self.buf.drain(..count * self.ratio);
        out
    }
}

/// One sample in -1..1 to signed 8-bit, scaled by `gain` and clipped.
pub fn to_s8(sample: f32, gain: f32) -> i8 {
    (sample * 127.0 * gain).round_ties_even().clamp(-127.0, 127.0) as i8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, frames: usize) -> Vec<Frame> {
        (0..frames)
            .map(|n| {
                let v = (2.0 * std::f64::consts::PI * hz * n as f64 / 48000.0).sin() as f32;
                [v, v]
            })
            .collect()
    }

    fn peak(frames: &[Frame]) -> f32 {
        frames.iter().map(|f| f[0].abs()).fold(0.0, f32::max)
    }

    #[test]
    fn sixteen_input_frames_make_one_output_frame() {
        let mut d = Decimator::default();
        assert_eq!(d.process(&tone(100.0, 4800)).len(), 300);
    }

    #[test]
    fn output_count_does_not_depend_on_how_input_is_chunked() {
        let input = tone(160.0, 4800);
        let mut whole = Decimator::default();
        let expected = whole.process(&input);
        let mut pieces = Decimator::default();
        let mut got = Vec::new();
        for chunk in input.chunks(37) {
            got.extend(pieces.process(chunk));
        }
        assert_eq!(got.len(), expected.len());
        for (a, b) in got.iter().zip(&expected) {
            assert!((a[0] - b[0]).abs() < 1e-5);
        }
    }

    #[test]
    fn haptic_frequencies_pass_at_full_strength() {
        let mut d = Decimator::default();
        let out = d.process(&tone(160.0, 9600));
        assert!((peak(&out[20..]) - 1.0).abs() < 0.02);
    }

    #[test]
    fn frequencies_that_would_alias_are_removed() {
        let mut d = Decimator::default();
        let out = d.process(&tone(2600.0, 9600));
        assert!(peak(&out[20..]) < 0.01);
    }

    #[test]
    fn samples_scale_and_clip() {
        assert_eq!(to_s8(1.0, 1.0), 127);
        assert_eq!(to_s8(-1.0, 1.0), -127);
        assert_eq!(to_s8(0.5, 0.5), 32);
        assert_eq!(to_s8(1.0, 4.0), 127);
        assert_eq!(to_s8(0.0, 1.0), 0);
    }
}
