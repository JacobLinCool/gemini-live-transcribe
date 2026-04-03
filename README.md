# gemini-live-transcribe

Live transcription in your terminal with Gemini Live.

It can capture your microphone on macOS, Linux, and Windows. `system-audio` remains macOS-only and uses a Core Audio tap on the default output device.

## Highlights

- Cross-platform `microphone` capture on macOS, Linux, and Windows
- Separate transcript pane per source: `microphone` everywhere, `system-audio` on macOS
- `system-audio` uses a Core Audio tap on the default output device, not screen capture
- Relative timestamps in `HH:MM:SS |` format
- Live Gemini context token count in each pane title
- Session renewal with context compression and resumption enabled
- Persistent config in the OS-standard app config directory
- Per-session JSONL logs in the OS-standard app data directory

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

Add a custom transcription instruction:

```bash
gemini-live-transcribe \
  --source microphone \
  --instruction "reply with less than 3 words."
```

Print the config and log paths:

```bash
gemini-live-transcribe paths
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

Example config:

```toml
# Auto-generated default config for gemini-live-transcribe.
# Fill in api_key if you want to avoid the interactive prompt.

# api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3.1-flash-live-preview"
# instruction = "reply with less than 3 words."
# sources = ["microphone"]

[logs]
max_files = 100
```

Notes:

- `api_key` stores the Gemini API key
- `instruction` biases transcription output; if omitted, the runtime default is `reply with less than 3 words.`
- `sources` accepts `microphone` everywhere and `system-audio` on macOS
- `[logs].max_files` must be at least `1`
- existing config files are not overwritten
- CLI flags override config values

## Logs

Each source writes a JSONL session log under the OS-standard app data directory.

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

On Linux and Windows, both release archives and source builds currently support `microphone`. `system-audio` is still macOS-only.
