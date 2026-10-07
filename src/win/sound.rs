//! Capture click: clip2's click sound as an embedded WAV, played fire-and-forget (spec 17, lesson 18.7).
//!
//! `assets/click.wav` is clip2's `click.mp3` decoded once, offline, with its leading and trailing
//! silence trimmed (the original starts ~75 ms late), so nothing is decoded at runtime:
//! `ffmpeg -i click.mp3 -af "silenceremove=start_periods=1:start_threshold=-55dB:detection=peak,areverse,silenceremove=start_periods=1:start_threshold=-55dB:detection=peak,areverse" -c:a pcm_s16le assets/click.wav`
//!
//! Every copy restarts the sound (`SND_ASYNC` cuts the one still playing), so each copy is audible
//! however fast they come. The calls go through a dedicated thread, so opening the audio device
//! never runs on the UI thread; the caller only does a channel send.
use crate::win::util::guarded;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

static CLICK: &[u8] = include_bytes!("../../assets/click.wav");
static PLAYER: OnceLock<Option<Sender<()>>> = OnceLock::new();

fn play() {
    let t = Instant::now();
    // SAFETY: SND_MEMORY with a 'static buffer that outlives the playback.
    unsafe {
        let _ = PlaySoundW(PCWSTR(CLICK.as_ptr() as *const u16), None, SND_MEMORY | SND_NODEFAULT | SND_ASYNC);
    }
    crate::log_dbg!("click: PlaySound call took {} ms", t.elapsed().as_millis());
}

fn spawn_player() -> Option<Sender<()>> {
    let (tx, rx) = channel::<()>();
    std::thread::Builder::new()
        .name("sound".into())
        .spawn(move || {
            guarded("sound", || {
                while rx.recv().is_ok() {
                    play();
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
        // No player thread (could not be spawned, or it died): call it directly.
        play();
    }
}
