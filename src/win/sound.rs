//! Capture click: an embedded WAV played fire-and-forget (spec 17, lesson 18.7).
//!
//! Playback happens on its own thread, one complete click at a time (`SND_SYNC` there). With
//! `SND_ASYNC` straight from the capture path, every call cuts off the click still playing and
//! rapid repeats can end up silent or clipped; a dedicated thread keeps each click whole while the
//! caller only does a channel send, so capture is never delayed.
use crate::win::util::guarded;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

static CLICK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/click.wav"));
static PLAYER: OnceLock<Option<Sender<()>>> = OnceLock::new();

fn play(mode: SND_FLAGS) {
    // SAFETY: SND_MEMORY with a 'static buffer that outlives the playback.
    unsafe {
        let _ = PlaySoundW(PCWSTR(CLICK.as_ptr() as *const u16), None, SND_MEMORY | SND_NODEFAULT | mode);
    }
}

fn spawn_player() -> Option<Sender<()>> {
    let (tx, rx) = channel::<()>();
    std::thread::Builder::new()
        .name("sound".into())
        .spawn(move || {
            guarded("sound", || {
                while rx.recv().is_ok() {
                    play(SND_SYNC);
                    // Requests that arrived while this click was playing are merged into it.
                    while rx.try_recv().is_ok() {}
                }
            });
        })
        .ok()
        .map(|_| tx)
}

/// Plays the click. Returns immediately.
pub fn click() {
    let sent = PLAYER.get_or_init(spawn_player).as_ref().is_some_and(|tx| tx.send(()).is_ok());
    if !sent {
        // No player thread (could not be spawned, or it died): fall back to the one-shot call.
        play(SND_ASYNC);
    }
}
