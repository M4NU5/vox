# Development guide

## Prerequisites

- [Node.js](https://nodejs.org) 18+
- [Rust](https://rustup.rs) (stable)
- Tauri platform dependencies — follow
  <https://tauri.app/start/prerequisites/> for your OS. In short:
  - **Windows**: Microsoft C++ Build Tools + WebView2 (preinstalled on Win 11).
  - **macOS**: Xcode Command Line Tools (`xcode-select --install`).
  - **Linux (Debian/Ubuntu)**:
    ```bash
    sudo apt update
    sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
      libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
      libasound2-dev espeak-ng
    ```
    `libxdo-dev` is needed by the keystroke simulation crate, `libasound2-dev`
    by audio playback, and `espeak-ng` powers the System voice.

## Run

```bash
npm install
npm run dev
```

This launches the app with hot-reloading of the Rust core and the static UI.

## The Kokoro engine

Kokoro runs in `vox-engine`, a separate program. Its crate is
`src-tauri/engine`, a member of the Cargo workspace. It links the
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) static libraries, which
the `sherpa-onnx` crate downloads on its first build. Vox starts one when a
reading needs it and talks to it over stdin and stdout, using the JSON-lines
protocol in `src-tauri/engine/src/protocol.rs`. It stays loaded for ten minutes
after a reading, so the next one skips the model load.

It is separate because sherpa-onnx includes espeak-ng, which is
GPL-3.0-or-later. The app only depends on the crate's protocol and client, with
the `kokoro` feature off, so none of that code ends up in Vox itself.

`npm run dev` doesn't build it. Build it once:

```bash
cd src-tauri
cargo build -p vox-engine
```

It lands in `target/debug` next to the dev app, which is where Vox looks
for it. `VOX_ENGINE_EXE` overrides the location.

Installers bundle it as a Tauri sidecar. `scripts/build-engine.sh` builds it
into `src-tauri/binaries/`, and `--config src-tauri/tauri.engine.conf.json`
adds it to the bundle. That setting lives in its own file because tauri-build
refuses to build when a sidecar is missing, and a plain `cargo build` shouldn't
need the engine.

## Vox Dev on Windows, from WSL

`scripts/build-dev-windows.sh` cross-compiles **Vox Dev** and the Kokoro
engine with [cargo-xwin](https://github.com/rust-cross/cargo-xwin) (it also
needs `clang` and `lld`) and copies them to `%LOCALAPPDATA%\VoxDev`. Vox
Dev uses its own identifier (`src-tauri/tauri.dev.conf.json`), so its settings,
downloads, keychain entries and `vox.log` stay apart from an installed
Vox. On Windows the log is at `%APPDATA%\dev.harshal.vox.dev\vox.log`.

## Tests

```bash
cd src-tauri
cargo test
cargo clippy --all-targets
cargo test -p vox-engine     # the protocol, and the client against a fake engine
cargo clippy -p vox-engine --all-targets
```

Tests that need a real Piper binary and voice are ignored by default:

```bash
VOX_PIPER_EXE=/path/to/piper VOX_PIPER_MODEL=/path/to/voice.onnx \
  cargo test -- --ignored piper
```

So are the ones that need the Kokoro model. Unpack
[kokoro-multi-lang-v1_0.tar.bz2](https://github.com/k2-fsa/sherpa-onnx/releases/tag/tts-models)
somewhere, then:

```bash
export VOX_KOKORO_MODEL=/path/to/kokoro-multi-lang-v1_0
cargo test -p vox-engine -- --ignored
VOX_ENGINE_EXE=$PWD/target/debug/vox-engine cargo test -- --ignored kokoro
```

`VOX_KOKORO_ARCHIVE=/path/to/kokoro-multi-lang-v1_0.tar.bz2` also runs the
test that unpacks the archive.

`render_passage_with_piper` and `render_passage_with_kokoro` are ignored too.
They read a text file through the whole pipeline, write the audio to a WAV and
check every chunk was ready before real-time playback needed it. Set
`VOX_RENDER_TEXT` and `VOX_RENDER_WAV` as well, and run them with
`--release --nocapture`. `VOX_RENDER_SPEED` sets Kokoro's speed.

## Pronunciation eval

Before a reading is chunked, `src-tauri/src/normalise.rs` rewrites text into
the words an English Kokoro or Piper voice should say: URLs, paths, code
identifiers, markdown, numbers, dates, money, versions, units, abbreviations,
symbols and emoji. Acronyms are left alone: Kokoro already spells out words in
capitals. It is rule-based. The cloud voices and the system voice don't use
it.

`src-tauri/tests/pronunciation` measures it. `cases.tsv` holds about 230
tricky inputs, each with the spoken form a careful human reader would use.
`eval.py` reads every case through the normaliser, synthesises it with the
real `vox-engine` (Kokoro, voice `af_heart`), transcribes the audio with
Whisper large-v3-turbo through sherpa-onnx, and reports the word error rate
overall, per category, and for the worst cases.

You need [uv](https://docs.astral.sh/uv/) and about 1.5 GB of disk. The first
run downloads the Kokoro and Whisper archives, checks them against the SHA-256
pinned in `eval.py`, and builds the engine. Audio and transcripts are cached in
`~/.cache/vox-eval` (`VOX_EVAL_CACHE` moves it), so later runs only
redo the cases whose normalised text changed.

```bash
cd src-tauri
uv run tests/pronunciation/eval.py                  # dev split, normaliser on
uv run tests/pronunciation/eval.py --raw            # baseline, no normaliser
uv run tests/pronunciation/eval.py --off units      # without one rule group
uv run tests/pronunciation/eval.py --split test     # the held-out cases
uv run tests/pronunciation/eval.py --json out.json  # per-case results
```

A full run takes a few minutes on a fast CPU, most of it in Whisper.

The `split` column keeps the number honest. Tune the normaliser against the
`dev` cases only. About a fifth of the single-feature cases are `test`, and
the `fresh` cases are realistic mixed snippets written before any tuning.
Score both only when you are done. A change is worth keeping when it lowers the dev
WER and makes sense for text outside the set. Don't add a rule for one case.

The scorer puts the expected form and Whisper's transcript into one canonical
form before comparing them: Whisper's own English normaliser, digits compared
one by one, and Whisper's symbols turned back into the words that make it
write them (`$5`, `API`, `kokoro.rs`). It never sees the input text. Whisper
still mishears some proper nouns (Kokoro, Tauri), which sets a floor that no
normaliser can get under.

## Build installers

```bash
scripts/build-engine.sh
npm run build -- --config src-tauri/tauri.engine.conf.json
```

Artifacts land in `src-tauri/target/release/bundle/` (`.msi`/`.exe` on Windows,
`.dmg`/`.app` on macOS, `.AppImage`/`.deb` on Linux).

## Project layout

```
voxtray/
├── src/                     # Settings UI (plain HTML/CSS/JS)
│   ├── index.html
│   ├── main.js              # invokes Tauri commands
│   └── styles.css
├── src-tauri/
│   ├── src/
│   │   ├── main.rs          # binary entry point
│   │   ├── lib.rs           # app setup, tray, hotkeys, plugins
│   │   ├── settings.rs      # config file + OS keychain
│   │   ├── reader.rs        # capture selection → speak; hotkey actions
│   │   ├── normalise.rs     # text → words to say (URLs, numbers, code…)
│   │   ├── chunker.rs       # text → sentence-sized chunks
│   │   ├── reading.rs       # one reading: synthesis ahead, pause, skip, stop
│   │   ├── timeline.rs      # decoded PCM + play cursor (rodio source)
│   │   ├── player.rs        # rodio audio + local process control
│   │   ├── logging.rs       # vox.log (timings, never text)
│   │   ├── commands.rs      # #[tauri::command] handlers
│   │   └── tts/             # openai / elevenlabs / piper / kokoro / local engines
│   ├── engine/              # vox-engine: Kokoro in its own process
│   │   ├── src/protocol.rs  # the stdio protocol (JSON lines)
│   │   ├── src/client.rs    # the app's side of it
│   │   ├── src/main.rs      # the engine binary
│   │   └── src/kokoro.rs    # Kokoro via sherpa-onnx
│   ├── icons/               # generated from icon-source.png
│   ├── capabilities/        # Tauri permission grants
│   ├── tauri.conf.json
│   ├── tauri.dev.conf.json  # Vox Dev identity overlay
│   ├── tauri.engine.conf.json  # bundles vox-engine (release builds)
│   └── Cargo.toml
└── .github/workflows/release.yml
```

## How selection capture works

There is no reliable cross-platform "get the selected text" API. Instead the app:

1. Saves the current clipboard, then clears it (as a sentinel).
2. Synthesises the platform copy shortcut (`Ctrl/Cmd+C`) with the `enigo` crate,
   so the focused app copies its own selection.
3. Reads the clipboard back (`arboard`); empty means nothing was selected.
4. Optionally restores the original clipboard.

This works in virtually any app or website without per-app integrations.

## Regenerating icons

Edit `src-tauri/icons/generate_icon.py` (requires Python + Pillow), then:

```bash
python src-tauri/icons/generate_icon.py     # writes icon-source.png
npm run icon                                 # regenerates the platform set
```

## Releasing

Tag a version and push it; the GitHub Actions workflow builds and drafts a
release with installers for all three platforms:

```bash
git tag v0.1.0
git push origin v0.1.0
```

See `.github/workflows/release.yml`.

## Notes / known limitations

- Builds are **unsigned**. Users will see OS warnings; document code-signing
  before a public 1.0 if desired (Apple Developer ID + notarization, Windows
  Authenticode).
- Wayland restricts global shortcuts and input synthesis on some compositors.
- The local (System voice) engine quality depends on installed OS voices.
