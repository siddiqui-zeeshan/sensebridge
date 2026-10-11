//! The haptics the Test button plays: a short sequence that shows what the controller
//! can do, where a single tone would only show that it is on.

use std::f32::consts::{PI, TAU};

use crate::hid::HAPTICS_RATE;

/// One stereo frame, left then right, each -1 to 1.
pub type Frame = [f32; 2];

/// A burst of `hz` that starts at full strength and dies away.
fn tap(out: &mut [Frame], hz: f32, decay: f32, left: f32, right: f32) {
    for (n, frame) in out.iter_mut().enumerate() {
        let t = n as f32 / HAPTICS_RATE as f32;
        let v = (TAU * hz * t).sin() * (-t / decay).exp();
        frame[0] += v * left;
        frame[1] += v * right;
    }
}

/// A rumble that rises in pitch, swells and fades, and travels from left to right.
fn sweep(out: &mut [Frame], from_hz: f32, to_hz: f32, strength: f32) {
    let length = out.len() as f32;
    let mut phase = 0.0f32;
    for (n, frame) in out.iter_mut().enumerate() {
        let along = n as f32 / length;
        phase += TAU * (from_hz + (to_hz - from_hz) * along) / HAPTICS_RATE as f32;
        let v = phase.sin() * (PI * along).sin() * strength;
        frame[0] += v * (1.0 - along).sqrt();
        frame[1] += v * along.sqrt();
    }
}

/// Left tap, right tap, a rumble rolling left to right, then a thud on both sides.
pub fn test_pattern() -> Vec<Frame> {
    let at = |seconds: f32| (seconds * HAPTICS_RATE as f32) as usize;
    let mut out = vec![[0.0f32; 2]; at(1.35)];
    tap(&mut out[at(0.00)..at(0.15)], 220.0, 0.025, 0.9, 0.0);
    tap(&mut out[at(0.18)..at(0.33)], 220.0, 0.025, 0.0, 0.9);
    sweep(&mut out[at(0.40)..at(1.00)], 60.0, 180.0, 0.8);
    tap(&mut out[at(1.08)..at(1.35)], 70.0, 0.06, 0.95, 0.95);
    // Settle to silence, so the last report leaves nothing ringing.
    let fade = at(0.02);
    let end = out.len();
    for (n, frame) in out[end - fade..].iter_mut().enumerate() {
        let keep = 1.0 - (n + 1) as f32 / fade as f32;
        frame[0] *= keep;
        frame[1] *= keep;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loudest(frames: &[Frame], side: usize) -> f32 {
        frames.iter().map(|f| f[side].abs()).fold(0.0, f32::max)
    }

    #[test]
    fn is_a_second_or_so_within_range_and_ends_silent() {
        let pattern = test_pattern();
        let seconds = pattern.len() as f32 / HAPTICS_RATE as f32;
        assert!((1.0..2.0).contains(&seconds), "{seconds} s");
        assert!(pattern.iter().flatten().all(|v| v.abs() <= 1.0));
        assert_eq!(pattern.last(), Some(&[0.0, 0.0]));
    }

    #[test]
    fn is_more_than_one_tone_on_both_sides() {
        let pattern = test_pattern();
        let at = |seconds: f32| (seconds * HAPTICS_RATE as f32) as usize;
        // The taps are one side each.
        let (first, second) = (&pattern[..at(0.15)], &pattern[at(0.18)..at(0.33)]);
        assert!(loudest(first, 0) > 0.5 && loudest(first, 1) == 0.0);
        assert!(loudest(second, 1) > 0.5 && loudest(second, 0) == 0.0);
        // The rumble starts on the left and ends on the right.
        let (start, end) = (&pattern[at(0.42)..at(0.48)], &pattern[at(0.92)..at(0.98)]);
        assert!(loudest(start, 0) > 2.0 * loudest(start, 1));
        assert!(loudest(end, 1) > 2.0 * loudest(end, 0));
        // And there is quiet between the parts.
        assert!(loudest(&pattern[at(1.02)..at(1.07)], 0) < 0.01);
    }
}
