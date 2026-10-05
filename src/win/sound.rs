//! Capture click: an embedded WAV played fire-and-forget (spec 17, lesson 18.7).
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

static CLICK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/click.wav"));

pub fn click() {
    // SAFETY: SND_MEMORY with a 'static buffer that outlives the async playback.
    unsafe {
        let _ = PlaySoundW(PCWSTR(CLICK.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}
