# Architecture Overview

`gemini-live-transcribe` is a single-binary terminal application for live transcription with Gemini Live. It captures one or more local audio sources, keeps a low-latency draft stream per source, runs a second-pass finalizer stream per source, and renders the resulting turn lifecycle in a minimal ratatui interface.

## Domains

- Capture: local audio acquisition and normalization for cross-platform `microphone` input and macOS-only `system-audio`
- Live session transport: Gemini Live WebSocket setup, audio upload, inbound event parsing, tool calling, session resumption, and context-window compression
- TUI state and rendering: per-source transcript panes, draft/pending/final/failed turn lifecycle, status notices, controls, token/context display, and relative timestamps
- Local persistence: home-directory config resolution, JSONL session logging, and finalized transcript JSON export

## Layers And Dependency Direction

- `src/main.rs`: top-level composition root. Resolves configuration, starts capture workers, draft/finalizer worker pairs per source, and runs the TUI event loop.
- `src/capture.rs`: source-specific audio capture. Converts local device input or Core Audio tap output into normalized mono `AudioChunk` values at `16 kHz`. `microphone` uses CPAL across platforms. `system-audio` is macOS-only and is captured from the default output device through a private aggregate device, not through display capture.
- `src/transcriber.rs`: Gemini Live boundary. Builds setup payloads, sends PCM audio and tool responses, parses inbound server messages, and emits typed session events.
- `src/ui.rs`: ratatui surface. Owns `App`, `AppEvent`, per-source pane state, turn lifecycle rendering, timestamp formatting, wrapping behavior, and token/context presentation.
- `src/session_log.rs`: JSONL audit trail for outbound setup/audio lifecycle events and inbound Gemini Live messages.
- `src/transcript.rs`: runtime transcription profile and transcript-export state model.
- `src/export.rs`: finalized transcript JSON export helper.
- `src/paths.rs`: canonical OS-standard paths for config, logs, and transcript exports.

Dependency direction is strictly inward:

1. capture/transcriber/session_log/paths do not depend on ui
2. `main.rs` is the only place that composes capture, transcriber, logging, and UI together
3. Gemini Live, cpal, Core Audio taps, platform directory resolution, and terminal I/O are boundary integrations, not core state owners

## Cross-Cutting Concerns

- Source isolation: each selected source owns an independent capture path, draft session, finalizer session, transcript pane, and log files
- Failure visibility: permission issues, WebSocket failures, and capture errors should surface explicitly in the UI and logs
- Operator-first readability: transcript density, stable timestamps, and honest per-source status matter more than decorative structure
- Session durability: session resumption and context-window compression are enabled so long-running streams survive socket churn
- Final transcript control: finalized turn text is produced by a tool-calling second pass, not by treating `inputAudioTranscription` as authoritative output

## Entry Points

- `cargo run`: interactive app startup with optional prompts for API key and source selection
- `cargo run -- paths`: prints the resolved config, logs, and transcript-export paths and exits
- `tests/paths_cli.rs`: CLI integration test for the `paths` subcommand
- unit tests in `src/*.rs`: behavior checks for capture batching, transcriber parsing, TUI layout, config resolution, and logging

## Related Docs

- [Frontend/TUI Surface](docs/FRONTEND.md)
- [Interaction And Visual Rules](docs/DESIGN.md)
- [Design Rationale Index](docs/design-docs/index.md)
- [Product Tradeoffs](docs/PRODUCT_SENSE.md)
- [Reliability Requirements](docs/RELIABILITY.md)
- [Security Requirements](docs/SECURITY.md)
