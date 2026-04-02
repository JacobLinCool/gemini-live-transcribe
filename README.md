# gemini-live-transcribe

Live transcription in your terminal with Gemini Live on macOS.

It can capture your microphone, system audio, or both at the same time, then render each source in its own TUI pane with relative timestamps and live Gemini context usage.

## Highlights

- Separate transcript pane per source: `microphone` and `system-audio`
- Relative timestamps in `HH:MM:SS |` format
- Live Gemini context token count in each pane title
- Session renewal with context compression and resumption enabled
- Persistent config in `~/.gemini-live-transcribe/config.toml`
- Per-session JSONL logs in `~/.gemini-live-transcribe/logs`

## Requirements

- macOS 15 or newer
- Xcode command line tools
- A Gemini API key
- Screen Recording permission if you want `system-audio`
- Microphone permission if you want `microphone`

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

The installer currently supports macOS `arm64` and `x86_64`.

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

Capture both microphone and system audio:

```bash
gemini-live-transcribe --source microphone --source system-audio
```

Add a custom transcription instruction:

```bash
gemini-live-transcribe \
  --source microphone \
  --instruction "Keep filler words and do not rewrite numbers."
```

Print the config and log paths:

```bash
gemini-live-transcribe paths
```

Update an installed binary to the latest GitHub release:

```bash
gemini-live-transcribe update
```

## First Run Notes

The first time you capture audio, macOS may ask for permissions.

- `system-audio` needs Screen Recording permission for your terminal or host app
- `microphone` needs Microphone permission

If permissions were denied earlier, re-enable them in macOS System Settings and restart the app.

## Configuration

The app reads `~/.gemini-live-transcribe/config.toml` on startup.
If the file does not exist yet, it creates a default skeleton automatically.

Resolution order is:

1. CLI flags
2. Environment variables where supported, currently `GEMINI_API_KEY`
3. `~/.gemini-live-transcribe/config.toml`
4. Interactive prompt

Example config:

```toml
# Auto-generated default config for gemini-live-transcribe.
# Fill in api_key if you want to avoid the interactive prompt.

# api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3.1-flash-live-preview"
# instruction = "Keep filler words and do not rewrite numbers."
# sources = ["microphone"]

[logs]
max_files = 100
```

Notes:

- `api_key` stores the Gemini API key
- `instruction` biases transcription output
- `sources` accepts `microphone` and `system-audio`
- `[logs].max_files` must be at least `1`
- existing config files are not overwritten
- CLI flags override config values

## Logs

Each source writes a JSONL session log under `~/.gemini-live-transcribe/logs`.

Logs include:

- session lifecycle events
- full inbound Gemini Live events
- outbound setup and stream-end events

Large base64 audio payloads are sanitized before being written to disk.
Log retention is count-based: after a new session starts, the app removes the oldest `*.jsonl` files until only `max_files` remain.

## Controls

- `q`: quit

## Build From Source

If you are developing locally, run:

```bash
cargo run
```

Or pass flags directly:

```bash
cargo run -- --source microphone --source system-audio
```
