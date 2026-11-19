# Privacy

Vox has no accounts, no telemetry, no analytics and no server of its own. What leaves your computer depends
only on the voice engine you pick.

## What is sent, and where

| Engine | What leaves your computer |
| --- | --- |
| **System voice** | Nothing. Fully offline. |
| **Local HD (Piper)** | Nothing you read. On first use it downloads the Piper engine from [github.com/rhasspy/piper](https://github.com/rhasspy/piper/releases) and your chosen voice from [huggingface.co/rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices). After that it runs offline. |
| **Kokoro (beta)** | Nothing you read. On first use it downloads the 350 MB Kokoro model from [github.com/k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx/releases/tag/tts-models). After that it runs offline. The Kokoro engine installed with Vox makes the speech on your machine. |
| **OpenAI TTS** | The selected text and your API key go to `api.openai.com` to make the audio. [OpenAI's privacy policy](https://openai.com/policies/privacy-policy) applies. |
| **ElevenLabs** | The selected text and your API key go to `api.elevenlabs.io`. [ElevenLabs' privacy policy](https://elevenlabs.io/privacy) applies. |

## What stays on your computer

- **API keys** are kept in your operating system's keychain (Windows Credential Manager, macOS Keychain, Linux
  Secret Service), never in a plain file.
- **Settings** (engine, voice, hotkeys, speed) are in a `settings.json` file in your user config folder, under
  `dev.harshal.vox`.
- **Downloaded voices and engines** are in your user data folder, under `dev.harshal.vox`.
- **A log of the last run** (`vox.log`, next to `settings.json`) records timings and errors. It never
  contains the text you read, and it is replaced each time Vox starts.
- **Your clipboard:** Vox reads your selection by sending the copy shortcut. With "restore clipboard" on
  (the default), your previous clipboard text is put back afterwards.

To remove everything, uninstall Vox, delete the two `dev.harshal.vox` folders and remove the Vox
entries from your keychain.
