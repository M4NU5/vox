//! Capturing the current selection and reading it aloud.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings as EnigoSettings};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::chunker;
use crate::latency::{self, Stage};
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

/// Simulate the platform copy shortcut so the focused app copies its selection.
///
/// Because the read hotkey (e.g. Ctrl+Alt+R) is usually still physically held
/// when this runs, we first send key-up for every modifier to neutralise the
/// OS modifier state — otherwise a stray Alt/Shift turns our synthetic Ctrl+C
/// into Ctrl+Alt+C and nothing gets copied.
fn simulate_copy() -> Result<(), String> {
    let mut enigo = Enigo::new(&EnigoSettings::default()).map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;

    // Release any modifiers the user is still holding from the hotkey.
    for k in [Key::Alt, Key::Shift, Key::Control, Key::Meta] {
        let _ = enigo.key(k, Direction::Release);
    }
    std::thread::sleep(MODIFIER_SETTLE);

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
    // Small delay so the emptied clipboard settles before we send copy.
    std::thread::sleep(Duration::from_millis(40));

    copy()?;

    // Poll the clipboard: apps service the copy command asynchronously, and
    // slower ones (browsers, Electron apps) can take several hundred ms.
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(40));
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

/// Start reading `text` aloud, replacing anything already playing.
pub fn speak_text(app: &AppHandle, settings: &Settings, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        latency::cancel();
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
            latency::cancel();
            crate::pill::failed();
            return notify(app, "Could not read text", &e);
        }
        // The system voice speaks the whole text itself.
        None => {
            latency::cancel();
            match tts::local::speak(settings, text) {
                Ok(child) => state.player.set_local_child(child),
                Err(e) => notify(app, "Could not read text", &e),
            }
            return;
        }
    };

    latency::mark(Stage::Open);
    let interrupt = engine.interrupter();
    let handle = app.clone();
    let start = move || {
        let chunks = chunker::chunk(text);
        latency::mark(Stage::Chunk);
        Reading::start(
            chunks,
            move |chunk| engine.synthesize(chunk),
            interrupt,
            move |reading, outcome| finished(&handle, reading, outcome),
        )
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
        latency::cancel();
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
        latency::cancel();
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

/// Capture the selection and read it. Runs the slow work on a worker thread.
pub fn read_selection(app: &AppHandle) {
    latency::start(Instant::now());
    let app = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<AppState> = app.state();
        let settings = {
            match state.settings.lock() {
                Ok(g) => g.clone(),
                Err(_) => Settings::default(),
            }
        };

        let captured = capture_selection(settings.restore_clipboard);
        if let Ok(text) = &captured {
            latency::mark(Stage::Capture);
            log::info!("captured {} chars", text.chars().count());
        }
        match captured {
            Ok(text) => speak_text(&app, &settings, &text),
            Err(e) => {
                latency::cancel();
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
    latency::cancel();
    stop_all(app);
    crate::sync_skip_hotkeys(app);
}

/// Time to first sound through the real pieces: clipboard, chunker, Kokoro
/// engine, reading worker and audio output. Only the app answering Ctrl+C
/// is simulated. Plays a moment of each reading on the default device.
///
/// Needs `VOXTRAY_ENGINE_EXE` and `VOXTRAY_KOKORO_MODEL`. Run with
/// `cargo test --lib first_sound -- --ignored --nocapture --test-threads 1`.
#[cfg(test)]
mod first_sound {
    use super::*;
    use crate::latency::Report;
    use crate::reading::Outcome;
    use crate::tts::kokoro::{self, engine_exe};
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

    fn model() -> PathBuf {
        PathBuf::from(std::env::var_os("VOXTRAY_KOKORO_MODEL").expect("set VOXTRAY_KOKORO_MODEL"))
    }

    fn player() -> &'static Player {
        static PLAYER: std::sync::OnceLock<Player> = std::sync::OnceLock::new();
        PLAYER.get_or_init(Player::new)
    }

    /// One read of `text`, from key-down to the first sample played.
    fn read(text: &'static str) -> Report {
        let (exe, model) = (engine_exe().unwrap(), model());
        std::thread::sleep(TAP);
        latency::start(Instant::now());
        let copy = move || {
            // As simulate_copy does, without sending keys.
            std::thread::sleep(MODIFIER_SETTLE);
            std::thread::spawn(move || {
                std::thread::sleep(APP_COPY);
                Clipboard::new().unwrap().set_text(text).unwrap();
            });
            Ok(())
        };
        let captured = capture_with(true, copy).unwrap();
        assert_eq!(captured, text);
        latency::mark(Stage::Capture);

        // As speak_text does.
        let mut engine =
            ChunkEngine::Kokoro(kokoro::Engine::open_local(&exe, &model, "af_heart", 1.0).unwrap());
        latency::mark(Stage::Open);
        let chunks = chunker::chunk(text);
        latency::mark(Stage::Chunk);
        let (tx, rx) = mpsc::channel();
        let reading = Reading::start(
            chunks,
            move |chunk| engine.synthesize(chunk),
            None,
            move |_: &Reading, outcome| tx.send(outcome).unwrap(),
        );
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

    /// Runs `n` reads and prints the median of each stage.
    fn measure(name: &str, text: &'static str, cold: bool, n: usize) {
        let reports: Vec<Report> = (0..n)
            .map(|_| {
                if cold {
                    kokoro::forget_warm_engine();
                } else {
                    // Make sure the engine is loaded and has synthesised once.
                    let _ = read("Warm up.");
                }
                let r = read(text);
                println!("  {name}: {r}");
                r
            })
            .collect();
        let stages = [
            Stage::Capture,
            Stage::Open,
            Stage::Chunk,
            Stage::Ready,
            Stage::Synth,
            Stage::Decode,
            Stage::Play,
        ];
        let parts: Vec<String> = stages
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
        let first = chunker::chunk(LONG)[0].chars().count();
        println!(
            "short {} chars, long first chunk {first} chars",
            SHORT.chars().count()
        );
        measure("short warm", SHORT, false, n);
        measure("long warm", LONG, false, n);
        measure("short cold", SHORT, true, n.min(3));
        measure("long cold", LONG, true, n.min(3));
    }
}
