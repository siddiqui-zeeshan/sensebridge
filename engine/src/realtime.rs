//! Keeps the engine's timing steady while the rest of the machine is busy (a game is).

use std::ffi::c_void;

const HIGH_PRIORITY_CLASS: u32 = 0x80;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
}

/// Raise this process above normal work and ask for 1 ms timer resolution, which the
/// 1 ms pacing of audio transfers depends on. Returns whether the priority was raised.
pub fn boost() -> bool {
    unsafe {
        timeBeginPeriod(1);
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) != 0
    }
}
