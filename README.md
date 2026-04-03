# gemini-live-transcribe

Live transcription in your terminal with Gemini Live.

It can capture your microphone on macOS, Linux, and Windows. `system-audio` remains macOS-only and uses a Core Audio tap on the default output device.

## Highlights

- Cross-platform `microphone` capture on macOS, Linux, and Windows
- Separate transcript pane per source: `microphone` everywhere, `system-audio` on macOS
- `system-audio` uses a Core Audio tap on the default output device, not screen capture
- Relative timestamps in `HH:MM:SS |` format
- Draft and finalized transcript lifecycle per source
- Live Gemini context token count in each pane title
- Session renewal with context compression and resumption enabled
- Persistent config in the OS-standard app config directory
- Per-session JSONL logs in the OS-standard app data directory
- Final transcript JSON export on normal shutdown

## Requirements

- A Gemini API key
- Microphone permission if you want `microphone`
- macOS 15 or newer plus Xcode command line tools if you want `system-audio`
- System Audio Recording permission if you want `system-audio` on macOS

## Install

Install the latest release into `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/JacobLinCool/gemini-live-transcribe/main/install.sh | bash
```

If you want a different install location:

```bash
curl -fsSL https://raw.githubusercontent.com/JacobLinCool/gemini-live-transcribe/main/install.sh \
  | GEMINI_LIVE_TRANSCRIBE_INSTALL_DIR="$HOME/bin" bash
```

The install script supports macOS `arm64`/`x86_64` and Linux `x86_64`.
The `update` subcommand supports macOS `arm64`/`x86_64`, Linux `x86_64`, and Windows `x86_64`.

## Quick Start

Launch the app:

```bash
gemini-live-transcribe
```

If `GEMINI_API_KEY` is not set, the app will prompt for it.
If you do not preselect sources, it will ask which audio sources to capture.

You can also pass the API key explicitly:

```bash
gemini-live-transcribe --token "$GEMINI_API_KEY"
```

## Common Usage

Capture both microphone and system audio on macOS:

```bash
gemini-live-transcribe --source microphone --source system-audio
```

Add separate draft and finalizer instructions:

```bash
gemini-live-transcribe \
  --source microphone \
  --draft-instruction "reply with less than 3 words." \
  --finalizer-instruction "prefer speaker labels when obvious."
```

Print the config and log paths:

```bash
gemini-live-transcribe paths
```

Record per-source debug WAVs of the exact audio sent to Gemini:

```bash
gemini-live-transcribe --debug
```

Update an installed binary to the latest GitHub release:

```bash
gemini-live-transcribe update
```

On Windows, install from the published release zip first, then use `gemini-live-transcribe update` for later upgrades.

## First Run Notes

The first time you capture audio, your OS may ask for permissions.

- `microphone` needs microphone permission
- `system-audio` needs System Audio Recording permission for your terminal or host app on macOS

If permissions were denied earlier, re-enable them in your OS settings and restart the app.

## Configuration

The app reads a per-user config file from the OS-standard app config directory on startup.
If the file does not exist yet, it creates a default skeleton automatically.

Resolution order is:

1. CLI flags
2. Environment variables where supported, currently `GEMINI_API_KEY`
3. the generated per-user config file
4. Interactive prompt

`--debug` is CLI-only and writes per-source `16 kHz mono PCM` WAV dumps under the OS-standard `debug/` data directory.

Example config:

```toml
# Auto-generated default config for gemini-live-transcribe.
# Fill in api_key if you want to avoid the interactive prompt.

# api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3.1-flash-live-preview"
# draft_instruction = "reply with less than 3 words."
# finalizer_instruction = "prefer speaker labels when obvious."
# sources = ["microphone"]

[transcription]
primary_language = "zh-Hant"
allowed_languages = ["en"]
keep_disfluencies = true
collapse_self_corrections = false
dedupe_immediate_repetition = false
numeral_policy = "preserve"

[logs]
max_files = 100
```

Notes:

- `api_key` stores the Gemini API key
- `[transcription]` controls the finalized transcript shape
- `primary_language` is the fallback output language for unsupported spoken languages
- `allowed_languages` may remain as spoken in the finalized transcript
- `draft_instruction` controls the low-latency first pass
- `finalizer_instruction` adds extra operator guidance to the second pass
- `--debug` writes `debug/<session_id>/<source>.wav` plus a small metadata JSON for capture diagnostics
- `sources` accepts `microphone` everywhere and `system-audio` on macOS
- `[logs].max_files` must be at least `1`
- existing config files are not overwritten
- CLI flags override config values

## Logs

Each source/stream pair writes a JSONL session log under the OS-standard app data directory.

Logs include:

- session lifecycle events
- full inbound Gemini Live events
- outbound setup and stream-end events

Large base64 audio payloads are sanitized before being written to disk.
Log retention is count-based: after a new session starts, the app removes the oldest `*.jsonl` files until only `max_files` remain.

On normal shutdown, the app also writes a transcript export to the OS-standard `transcripts/` data directory as `<session_id>.json`.
This file contains the finalized text plus fallback state for failed or unfinished turns.

## Controls

- `q`: quit
  The app stops capture, drains pending finalization for a bounded period, then writes the transcript JSON export.

## Build From Source

If you are developing locally, run:

```bash
cargo run
```

Or pass flags directly:

```bash
cargo run -- --source microphone --source system-audio
```

On Linux and Windows, both release archives and source builds currently support `microphone`. `system-audio` is still macOS-only.
