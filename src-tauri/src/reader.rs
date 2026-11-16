//! Capturing the current selection and reading it aloud.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings as EnigoSettings};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::chunker;
use crate::latency::{self, Stage, TraceId};
use crate::player::Player;
use crate::reading::{Outcome, Reading, Slot};
use crate::settings::Settings;
use crate::tts::{self, ChunkEngine};

/// Shared application state, managed by Tauri and reachable from hotkey handlers.
pub struct AppState {
    pub player: Arc<Player>,
    pub settings: Arc<Mutex<Settings>>,
    /// The chunked reading in progress, if any.
    pub reading: Mutex<Slot>,
}

/// How long released modifiers get to settle before the copy shortcut.
const MODIFIER_SETTLE: Duration = Duration::from_millis(60);

/// How often the clipboard is checked for the copied selection.
const POLL: Duration = Duration::from_millis(10);

/// Simulate the platform copy shortcut so the focused app copies its selection.
///
/// Because the read hotkey (e.g. Ctrl+Alt+R) is usually still physically held
/// when this runs, we first send key-up for every modifier to neutralise the
/// OS modifier state — otherwise a stray Alt/Shift turns our synthetic Ctrl+C
/// into Ctrl+Alt+C and nothing gets copied. After a tap of the read key
/// nothing is held, so that step and its wait are skipped.
fn simulate_copy() -> Result<(), String> {
    let mut enigo = Enigo::new(&EnigoSettings::default()).map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;

    // Release any modifiers the user is still holding from the hotkey.
    if modifiers_held() {
        for k in [Key::Alt, Key::Shift, Key::Control, Key::Meta] {
            let _ = enigo.key(k, Direction::Release);
        }
        std::thread::sleep(MODIFIER_SETTLE);
    }

    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| e.to_string())?;
    enigo
        .key(Key::Unicode('c'), Direction::Click)
        .map_err(|e| e.to_string())?;
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether Alt, Shift or Windows is down, any of which would turn our Ctrl+C
/// into another shortcut. A held Ctrl is fine: we press it anyway.
#[cfg(windows)]
fn modifiers_held() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    [VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|&k| unsafe { GetAsyncKeyState(i32::from(k)) } as u16 & 0x8000 != 0)
}

/// Elsewhere there's no cheap check, so always release them.
#[cfg(not(windows))]
fn modifiers_held() -> bool {
    true
}

/// The clipboard's change counter. Reading it doesn't open the clipboard,
/// so polling it can't get in the way of the app writing the selection.
#[cfg(windows)]
fn clipboard_sequence() -> Option<u32> {
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn clipboard_sequence() -> Option<u32> {
    None
}

/// Capture the currently-selected text by copying it to the clipboard.
///
/// The clipboard is cleared first so an empty result reliably means
/// "nothing was selected", and the previous contents are restored afterwards
/// when `restore` is set.
fn capture_selection(restore: bool) -> Result<String, String> {
    capture_with(restore, simulate_copy)
}

/// [`capture_selection`], with `copy` standing in for the copy shortcut.
fn capture_with(
    restore: bool,
    copy: impl FnOnce() -> Result<(), String>,
) -> Result<String, String> {
    let mut clipboard = Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
    let previous = clipboard.get_text().ok();

    // Sentinel so we can detect whether the copy produced anything.
    let _ = clipboard.set_text(String::new());
    let cleared = clipboard_sequence();

    copy()?;

    // Poll the clipboard: apps service the copy command asynchronously, and
    // slower ones (browsers, Electron apps) can take several hundred ms.
    // Where the change counter exists, the clipboard is only opened once it
    // has moved.
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if cleared.is_some() && clipboard_sequence() == cleared {
            continue;
        }
        if let Ok(t) = clipboard.get_text() {
            if !t.is_empty() {
                text = t;
                break;
            }
        }
    }

    if restore {
        if let Some(prev) = previous {
            let _ = clipboard.set_text(prev);
        }
    }

    Ok(text)
}

pub fn notify(app: &AppHandle, title: &str, body: &str) {
    log::warn!("{title}: {}", crate::logging::loggable(body));
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Start reading `text` aloud, replacing anything already playing. Its
/// stages go on read `trace`'s timing.
pub fn speak_text(app: &AppHandle, settings: &Settings, text: &str, trace: TraceId) {
    let text = text.trim();
    if text.is_empty() {
        latency::cancel(trace);
        notify(app, "Voxtray", "No text was selected.");
        return;
    }

    // Stop whatever is currently playing (any engine) before starting anew.
    let generation = stop_all(app);

    let state: tauri::State<AppState> = app.state();
    log::info!(
        "opening {:?} for {} chars",
        settings.provider,
        text.chars().count()
    );
    let mut engine = match ChunkEngine::open(app, settings) {
        Some(Ok(engine)) => engine,
        Some(Err(e)) => {
            latency::cancel(trace);
            crate::pill::failed();
            return notify(app, "Could not read text", &e);
        }
        // The system voice speaks the whole text itself.
        None => {
            latency::cancel(trace);
            match tts::local::speak(settings, text) {
                Ok(child) => state.player.set_local_child(child),
                Err(e) => notify(app, "Could not read text", &e),
            }
            return;
        }
    };

    latency::mark(trace, Stage::Open);
    let interrupt = engine.interrupter();
    let handle = app.clone();
    let start = move || {
        let chunks = chunker::chunk(text);
        latency::mark(trace, Stage::Chunk);
        let synth = move |chunk: &str| {
            let pcm = engine.synthesize(chunk, trace);
            latency::mark(trace, Stage::Decode);
            pcm
        };
        let reading = Reading::start(chunks, synth, interrupt, move |reading, outcome| {
            finished(&handle, reading, outcome)
        });
        reading.shared.set_trace(trace);
        reading
    };
    // Opening the engine can take a while (the first Piper or Kokoro
    // download). If Stop or another read happened meanwhile, drop this one.
    // Play is sent under the slot lock, like Stop in stop_all, so the audio
    // thread gets them in the same order as the slot changes.
    let installed = state.reading.lock().ok().and_then(|mut slot| {
        let reading = slot.install(generation, start)?;
        let played = state.player.play_timeline(reading.shared.clone());
        Some((reading, played))
    });
    let Some((reading, played)) = installed else {
        log::info!("start cancelled while opening the engine");
        return;
    };
    if let Err(e) = played {
        latency::cancel(trace);
        reading.stop();
        crate::pill::failed();
        notify(app, "Could not read text", &e);
    }
    crate::pill::refresh();
    crate::sync_skip_hotkeys(app);
}

/// Runs when a reading ends on its own, was stopped, or failed.
fn finished(app: &AppHandle, reading: &Reading, outcome: Outcome) {
    let state: tauri::State<AppState> = app.state();
    if let Ok(mut slot) = state.reading.lock() {
        slot.clear(reading.id);
    }
    crate::sync_skip_hotkeys(app);
    if let Outcome::Failed(e) = outcome {
        latency::cancel(reading.shared.trace());
        crate::pill::failed();
        notify(app, "Could not read text", &e);
    }
}

/// The reading still playing, paused or loading, if any.
pub fn active_reading(app: &AppHandle) -> Option<Arc<Reading>> {
    let state: tauri::State<AppState> = app.state();
    let current = state.reading.lock().ok()?.current();
    current.filter(|r| r.is_active())
}

/// The read hotkey: pause or resume the current reading, or if nothing is
/// being read, read the selection. A new selection needs Stop first.
pub fn trigger_read(app: &AppHandle) {
    if let Some(reading) = active_reading(app) {
        let state = reading.toggle_pause();
        log::info!("reading {}: {state:?}", reading.id);
        return;
    }
    log::info!("read hotkey: capturing selection");
    read_selection(app);
}

/// A read may be about to start: get a local engine loading in the
/// background, so it's ready sooner. Does nothing mid-reading, when the read
/// key pauses instead.
pub fn prepare(app: &AppHandle) {
    if active_reading(app).is_none() {
        let settings = current_settings(app);
        std::thread::spawn(move || tts::prepare(&settings));
    }
}

fn current_settings(app: &AppHandle) -> Settings {
    let state: tauri::State<AppState> = app.state();
    let settings = state.settings.lock().map(|g| g.clone());
    settings.unwrap_or_default()
}

/// Capture the selection and read it. Runs the slow work on a worker thread.
pub fn read_selection(app: &AppHandle) {
    let trace = latency::start(Instant::now());
    // The engine loads while the selection is copied.
    prepare(app);
    let app = app.clone();
    std::thread::spawn(move || {
        let settings = current_settings(&app);

        let captured = capture_selection(settings.restore_clipboard);
        if let Ok(text) = &captured {
            latency::mark(trace, Stage::Capture);
            log::info!("captured {} chars", text.chars().count());
        }
        match captured {
            Ok(text) => speak_text(&app, &settings, &text, trace),
            Err(e) => {
                latency::cancel(trace);
                notify(&app, "Could not capture selection", &e)
            }
        }
    });
}

/// Pause or resume the current reading. Does nothing when idle.
pub fn toggle_pause(app: &AppHandle) {
    if let Some(reading) = active_reading(app) {
        let state = reading.toggle_pause();
        log::info!("reading {}: {state:?}", reading.id);
    }
}

/// Skip the current reading 5 s forward or back.
pub fn skip(app: &AppHandle, forward: bool) {
    if let Some(reading) = active_reading(app) {
        let (pos, buffered) = reading.skip(forward);
        log::info!(
            "reading {}: skip to {:.1}s of {:.1}s synthesised",
            reading.id,
            pos.as_secs_f32(),
            buffered.as_secs_f32()
        );
    }
}

/// Stop everything. Returns the reading generation a new start should use.
fn stop_all(app: &AppHandle) -> u64 {
    let state: tauri::State<AppState> = app.state();
    let mut slot = state.reading.lock().unwrap_or_else(|e| e.into_inner());
    let generation = slot.stop();
    state.player.stop();
    drop(slot);
    generation
}

/// Stop any current playback (all engines) and cancel pending synthesis.
pub fn trigger_stop(app: &AppHandle) {
    latency::cancel_any();
    stop_all(app);
    crate::sync_skip_hotkeys(app);
}

/// Time to first sound through the real pieces: clipboard, chunker, Kokoro
/// engine, reading worker and audio output. Only the key presses and the app
/// answering Ctrl+C are simulated. Plays a moment of each reading on the
/// default device, and puts the clipboard's text back after each.
///
/// Needs `VOXTRAY_ENGINE_EXE` and `VOXTRAY_KOKORO_MODEL`. Run with
/// `cargo test --lib first_sound -- --ignored --nocapture --test-threads 1`.
#[cfg(test)]
mod first_sound {
    use super::*;
    use crate::latency::Report;
    use crate::reading::Outcome;
    use crate::tts::kokoro;
    use std::path::PathBuf;
    use std::sync::mpsc;

    const SHORT: &str =
        "The library closes early on Fridays, so please return your books before four o'clock.";

    const LONG: &str = "When the old harbour was finally rebuilt after the storm, the town \
council insisted that the new sea wall follow the line of the original stones, even though \
the engineers had argued for a straighter and cheaper route. The result is a wall that bends \
twice before it reaches the lighthouse. Visitors often ask why, and the answer is usually a \
story about the fishermen who refused to move their boats.

Today the harbour is quieter than it once was. A handful of boats still go out before dawn, \
but most of the moorings are taken by pleasure craft, and the old fish market has become a \
café that serves coffee to walkers on the coastal path. On a calm morning you can still hear \
the halyards ringing against the masts.";

    /// How long the simulated app takes to put the selection on the clipboard.
    const APP_COPY: Duration = Duration::from_millis(15);
    /// How long the read key is held for a tap.
    const TAP: Duration = Duration::from_millis(90);
    const VOICE: &str = "af_heart";

    fn path(var: &str) -> PathBuf {
        PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("set {var}")))
    }

    fn player() -> &'static Player {
        static PLAYER: std::sync::OnceLock<Player> = std::sync::OnceLock::new();
        PLAYER.get_or_init(Player::new)
    }

    /// One read of `text`, from the read key going down to the first sample
    /// played. The report counts from the key coming up, as the app does.
    fn read(text: &'static str) -> Report {
        let (exe, model) = (path("VOXTRAY_ENGINE_EXE"), path("VOXTRAY_KOKORO_MODEL"));
        // As the read key going down, then read_selection, do.
        let prepare = || {
            let (exe, model) = (exe.clone(), model.clone());
            std::thread::spawn(move || kokoro::prepare_local(&exe, &model, VOICE));
        };
        prepare();
        std::thread::sleep(TAP);
        let trace = latency::start(Instant::now());
        prepare();
        let copy = move || {
            // As simulate_copy does, without sending keys.
            if modifiers_held() {
                std::thread::sleep(MODIFIER_SETTLE);
            }
            std::thread::spawn(move || {
                std::thread::sleep(APP_COPY);
                Clipboard::new().unwrap().set_text(text).unwrap();
            });
            Ok(())
        };
        let captured = capture_with(true, copy).unwrap();
        assert_eq!(captured, text);
        latency::mark(trace, Stage::Capture);

        // As speak_text does.
        let engine = kokoro::Engine::open_local(&exe, &model, VOICE, 1.0).unwrap();
        let mut engine = ChunkEngine::Kokoro(engine);
        latency::mark(trace, Stage::Open);
        let chunks = chunker::chunk(text);
        latency::mark(trace, Stage::Chunk);
        let (tx, rx) = mpsc::channel();
        let synth = move |chunk: &str| {
            let pcm = engine.synthesize(chunk, trace);
            latency::mark(trace, Stage::Decode);
            pcm
        };
        let reading = Reading::start(chunks, synth, None, move |_: &Reading, outcome| {
            tx.send(outcome).unwrap()
        });
        reading.shared.set_trace(trace);
        player().play_timeline(reading.shared.clone()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(60);
        let report = loop {
            if let Some(r) = latency::last() {
                break r;
            }
            assert!(Instant::now() < deadline, "no sound");
            std::thread::sleep(Duration::from_millis(2));
        };
        std::thread::sleep(Duration::from_millis(250));
        reading.stop();
        player().stop();
        // The engine goes back to warm once its chunk in progress is done.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(60)),
            Ok(Outcome::Stopped)
        );
        report
    }

    fn median(mut v: Vec<u128>) -> u128 {
        v.sort_unstable();
        v[v.len() / 2]
    }

    /// Reads `text` `n` times and prints the median of the total and of each
    /// stage. Warm reads follow a short read on a fresh engine; cold reads
    /// start with no engine running.
    fn measure(name: &str, text: &'static str, cold: bool, n: usize) {
        let reports: Vec<Report> = (0..n)
            .map(|_| {
                kokoro::forget_warm_engine();
                if !cold {
                    let _ = read("Warm up.");
                }
                let r = read(text);
                println!("  {name}: {r}");
                r
            })
            .collect();
        let parts: Vec<String> = [
            Stage::Capture,
            Stage::Open,
            Stage::Chunk,
            Stage::Ready,
            Stage::Synth,
            Stage::Decode,
            Stage::Play,
        ]
        .iter()
        .map(|&s| {
            let ms = median(reports.iter().map(|r| r.stage(s).as_millis()).collect());
            format!("{s:?} {ms}")
        })
        .collect();
        let total = median(reports.iter().map(|r| r.total.as_millis()).collect());
        println!("MEDIAN {name}: total {total} ms | {}", parts.join(", "));
    }

    #[test]
    #[ignore = "needs voxtray-engine, the Kokoro model, a clipboard and speakers"]
    fn first_sound_timings() {
        let n: usize = std::env::var("VOXTRAY_RUNS").map_or(5, |s| s.parse().unwrap());
        for text in [SHORT, LONG] {
            let first = chunker::chunk(text)[0].chars().count();
            println!("{} chars, first chunk {first}", text.chars().count());
        }
        measure("short warm", SHORT, false, n);
        measure("long warm", LONG, false, n);
        measure("short cold", SHORT, true, n);
        measure("long cold", LONG, true, n);
    }
}
