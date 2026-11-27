# VEXA

**An always-listening, privacy-first voice assistant for your PC, powered by Claude.**

Keep working. Say *"Vexa"* anywhere in a sentence, and Vexa handles it in the background:

> "What's the closing price of Nvidia on Friday, **Vexa**?"
> "**Vexa**, best flight from Sydney to Delhi in December under 1000 dollars?"

A small wave appears at the bottom-centre of your screen to show the request was heard. The
answer appears in a card there and is read aloud. Vexa never takes focus, so it never interrupts
your typing.

## Privacy

- **Audio never leaves your PC, and is never written to disk.** Voice detection and speech-to-text
  run locally. Audio only exists in memory for the length of one sentence.
- **Only requests go to the cloud.** Claude sees the text of a sentence only when it contains the
  wake word. Everything else you say is transcribed locally, then discarded.
- **Nothing is logged.** No transcripts, answers or analytics are stored. Only your settings and
  the speech model are saved.
- Your API key is kept in Windows Credential Manager.

## How it works

```
Mic ─▶ voice detection ─▶ local speech-to-text ─▶ wake word? ─▶ Claude + web search ─▶ card + voice
       (Earshot, ~0% CPU)  (whisper.cpp, only     (on device)    (background thread)
                            while someone talks)
```

| Part | Choice | Why |
|---|---|---|
| App | Native Rust, Win32 | No browser engine: about 1 MB of UI code, low RAM |
| Voice detection | [Earshot](https://crates.io/crates/earshot) | Pure Rust, ~8 KB of state, faster than Silero |
| Speech-to-text | [whisper.cpp](https://github.com/ggerganov/whisper.cpp) `base.en` (q5) | Accurate on CPU, runs only on speech |
| Answers | Claude Opus 5.5 with web search | Current facts: prices, news, flights, weather |
| Voice | Windows built-in speech (SAPI) | Local and free |

## Install (preview)

1. Download `vexa-windows-x64` from the latest successful run on the
   [Actions](../../actions/workflows/build.yml) tab, unzip it and run `vexa.exe`.
2. Paste your [Anthropic API key](https://console.anthropic.com/) into the settings window.
3. On first run Vexa downloads the speech model (~57 MB), then shows *"Vexa is listening"*.

Right-click the tray icon to pause listening, open settings, turn on **Start with Windows**, or quit.

Say **"Vexa, stop"** to stop an answer that's being read aloud.

> Windows SmartScreen may warn about an unsigned app. Signed installers come in a later phase.

## Build from source (Windows)

1. Install [Rust](https://rustup.rs/), [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
   (choose *Desktop development with C++*), [CMake](https://cmake.org/download/) and
   [LLVM](https://github.com/llvm/llvm-project/releases) (`winget install LLVM.LLVM`).
2. Build and run:

   ```powershell
   cargo build --release
   .\target\release\vexa.exe
   ```

Useful modes for development:

```powershell
$env:VEXA_DEBUG=1; cargo run              # print what Vexa hears (console only, never saved)
cargo run -- --ask "weather in Sydney"    # ask Claude directly
cargo run -- --wav recording.wav          # run a recording through the listening pipeline
```

## Settings

Saved at `%LOCALAPPDATA%\Vexa\settings.json`:

| Key | Default | |
|---|---|---|
| `speak_answers` | `true` | Read answers aloud (cards always show) |
| `stt_model` | `base.en-q5_1` | `tiny.en-q5_1` is faster on older PCs, slightly less accurate |
| `stt_threads` | `0` | CPU threads for speech-to-text (0 = automatic) |
| `vad_threshold` | `0.5` | Raise to 0.6-0.7 in noisy rooms |

## Roadmap

- [x] **Phase 1:** wake word anywhere in a sentence, local speech-to-text, Claude with web search, wave overlay, spoken answers, tray
- [ ] **Phase 2:** smart mode (requests without the wake word), owner-voice check, ignore speaker audio, auto-pause in meetings
- [ ] **Phase 3:** accounts, free tier (20 requests/day) with no API key needed, signed installer, auto-update
- [ ] **Phase 4:** "show me" result cards, flights, finance, more tools
