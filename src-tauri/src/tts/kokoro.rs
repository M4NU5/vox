//! Kokoro: free, offline, neural voice. It runs in `vox-engine`, a
//! separate binary that ships next to Vox (see the `vox_engine`
//! crate). The model is fetched on first use (see [`crate::download`]).
//!
//! The engine stays loaded for [`KEEP_WARM`] after a reading, so the next
//! one starts without waiting about 2 s for the model. Stop doesn't kill it:
//! the chunk in progress finishes, is thrown away, and the engine goes back
//! to waiting.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tauri::AppHandle;
use vox_engine::{kokoro_lexicon, Client};

use super::warm::KeepWarm;
use crate::download;
use crate::latency::{self, Stage, TraceId};
use crate::settings::Settings;
use crate::timeline::Pcm;

/// How long an unused engine stays loaded.
const KEEP_WARM: Duration = Duration::from_secs(10 * 60);

/// The engine left by the last reading, while it is warm.
static IDLE: KeepWarm<Loaded> = KeepWarm::new("Kokoro engine", KEEP_WARM);

/// An engine process and the lexicon it loaded with the model.
struct Loaded {
    client: Client,
    lexicon: &'static str,
}

impl Loaded {
    /// Still running, with the lexicon `voice` needs.
    fn fits(&self, voice: &str) -> bool {
        self.lexicon == kokoro_lexicon(voice) && self.client.is_running()
    }
}

/// Held while taking the warm engine or starting one, so a reading and an
/// early start never both start an engine.
fn opening() -> std::sync::MutexGuard<'static, ()> {
    static OPENING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    OPENING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start an engine ahead of a likely reading, unless a fitting one is warm.
/// It loads the model in the background and waits, warm, for the reading.
/// Never downloads the model.
pub fn prepare(settings: &Settings) {
    if !download::kokoro_model_installed() {
        return;
    }
    if let (Ok(exe), Ok(model)) = (engine_exe(), download::kokoro_model_dir()) {
        prepare_local(&exe, &model, &settings.kokoro_voice);
    }
}

/// [`prepare`], with the engine and model given.
pub fn prepare_local(exe: &Path, model: &Path, voice: &str) {
    let _one_at_a_time = opening();
    if IDLE.holds(|loaded| loaded.fits(voice)) {
        return;
    }
    // Stops an engine with the other lexicon.
    drop(IDLE.take());
    log::info!("starting the Kokoro engine early");
    match Engine::spawn(exe, model, voice, 1.0) {
        // Dropping it keeps it warm.
        Ok(engine) => drop(engine),
        Err(e) => log::warn!(
            "early Kokoro start failed: {}",
            crate::logging::loggable(&e)
        ),
    }
}

/// A running Kokoro engine that synthesises one chunk at a time. Dropping it
/// keeps the engine warm for the next reading.
pub struct Engine {
    /// Only taken when the engine is dropped.
    loaded: Option<Loaded>,
    voice: String,
    speed: f32,
    /// A request failed, so the engine may be in a bad state. It isn't kept.
    failed: bool,
}

impl Engine {
    /// The warm engine if there is one. Otherwise download the model if
    /// needed and start an engine, returning before the model has loaded;
    /// the first chunk waits for it.
    pub fn open(app: &AppHandle, settings: &Settings) -> Result<Engine, String> {
        let exe = engine_exe()?;
        if !download::kokoro_model_installed() {
            crate::reader::notify(
                app,
                "Downloading Kokoro",
                "The Kokoro model (350 MB) downloads once. Reading starts when it's done.",
            );
        }
        let model = download::ensure_kokoro_model(app)?;
        Engine::open_local(&exe, &model, &settings.kokoro_voice, settings.speed)
    }

    /// The warm engine if there is one, otherwise a new one from `exe`.
    pub fn open_local(exe: &Path, model: &Path, voice: &str, speed: f32) -> Result<Engine, String> {
        let _one_at_a_time = opening();
        match Engine::reuse(voice, speed) {
            Some(engine) => {
                log::info!("Kokoro engine still warm");
                Ok(engine)
            }
            None => Engine::spawn(exe, model, voice, speed),
        }
    }

    pub fn spawn(exe: &Path, model: &Path, voice: &str, speed: f32) -> Result<Engine, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--model-dir").arg(model).args(["--voice", voice]);
        super::hide_console(&mut cmd);
        let loaded = Loaded {
            client: Client::spawn(cmd, "Kokoro")?,
            lexicon: kokoro_lexicon(voice),
        };
        Ok(Engine::new(loaded, voice, speed))
    }

    /// The engine the last reading left warm, if it is still running and
    /// loaded the lexicon `voice` needs. Otherwise it is stopped.
    fn reuse(voice: &str, speed: f32) -> Option<Engine> {
        let loaded = IDLE.take()?;
        if !loaded.fits(voice) {
            return None;
        }
        Some(Engine::new(loaded, voice, speed))
    }

    fn new(loaded: Loaded, voice: &str, speed: f32) -> Engine {
        Engine {
            loaded: Some(loaded),
            voice: voice.to_string(),
            speed: super::speed_param(speed, 0.5, 2.0) as f32,
            failed: false,
        }
    }

    /// Speak `text`, marking its stages on read `trace`'s timing.
    pub fn synthesize(&mut self, text: &str, trace: TraceId) -> Result<Pcm, String> {
        let loaded = self.loaded.as_mut().expect("only taken on drop");
        let ready = loaded.client.ready().map(|_| ());
        self.failed |= ready.is_err();
        ready?;
        latency::mark(trace, Stage::Ready);
        let wav = loaded.client.synthesize(text, &self.voice, self.speed);
        self.failed |= wav.is_err();
        latency::mark(trace, Stage::Synth);
        Pcm::decode(wav?)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            if !self.failed && loaded.client.is_running() {
                IDLE.put(loaded);
            }
        }
    }
}

/// Stop the warm engine, so the next reading starts a cold one.
#[cfg(test)]
pub fn forget_warm_engine() {
    drop(IDLE.take());
}

/// The engine binary: `VOX_ENGINE_EXE` if set (for development),
/// otherwise `vox-engine` next to Vox, where the installer puts it.
pub fn engine_exe() -> Result<PathBuf, String> {
    if let Some(exe) = std::env::var_os("VOX_ENGINE_EXE") {
        return Ok(PathBuf::from(exe));
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|me| {
            Some(
                me.parent()?
                    .join(format!("vox-engine{}", std::env::consts::EXE_SUFFIX)),
            )
        })
        .ok_or("could not locate the Kokoro engine")?;
    if exe.exists() {
        Ok(exe)
    } else {
        Err("The Kokoro engine (vox-engine) is missing. Reinstall Vox.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine from `VOX_ENGINE_EXE` and the model from
    /// `VOX_KOKORO_MODEL`. Run with `cargo test -- --ignored`.
    fn engine() -> Engine {
        let exe = engine_exe().expect("set VOX_ENGINE_EXE");
        let model = std::env::var_os("VOX_KOKORO_MODEL").expect("set VOX_KOKORO_MODEL");
        Engine::spawn(&exe, Path::new(&model), "af_heart", 1.0).expect("spawn Kokoro")
    }

    #[test]
    #[ignore = "needs vox-engine and the Kokoro model"]
    fn warm_engine_synthesises_chunks_in_order() {
        let mut engine = engine();
        let short = engine.synthesize("Hi.", TraceId::NONE).unwrap();
        let long = engine
            .synthesize(
                "This sentence is a good deal longer than that.",
                TraceId::NONE,
            )
            .unwrap();
        assert_eq!((short.channels, short.sample_rate), (1, 24_000));
        assert!(long.duration() > short.duration());
        assert!(engine.synthesize("...", TraceId::NONE).is_ok());
    }

    #[test]
    #[ignore = "needs vox-engine and the Kokoro model"]
    fn a_dropped_engine_is_reused_while_warm() {
        let mut engine = engine();
        engine.synthesize("Hi.", TraceId::NONE).unwrap();
        drop(engine);
        let mut again = Engine::reuse("af_bella", 1.2).expect("still warm");
        assert_eq!((again.voice.as_str(), again.speed), ("af_bella", 1.2));
        again.synthesize("Hello again.", TraceId::NONE).unwrap();
        drop(again);
        // A British voice needs the other lexicon, so it gets a new engine.
        assert!(Engine::reuse("bf_emma", 1.0).is_none());
    }

    #[test]
    #[ignore = "needs vox-engine and the Kokoro model"]
    fn stopping_mid_chunk_keeps_the_engine_warm() {
        use crate::reading::{Outcome, Reading};
        use crate::tts::ChunkEngine;

        let mut engine = ChunkEngine::Kokoro(engine());
        let interrupt = engine.interrupter();
        let long = "This sentence is long enough to keep the engine busy for a while, \
                    so the stop lands while it is still being synthesised.";
        let (tx, rx) = std::sync::mpsc::channel();
        let reading = Reading::start(
            vec![long.to_string(); 3],
            move |chunk| engine.synthesize(chunk, TraceId::NONE),
            interrupt,
            move |_: &Reading, outcome| tx.send(outcome).unwrap(),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        reading.stop();
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
        assert_eq!(outcome, Ok(Outcome::Stopped));
        let mut again = Engine::reuse("af_heart", 1.0).expect("still warm after stop");
        again.synthesize("Still here.", TraceId::NONE).unwrap();
    }
}
