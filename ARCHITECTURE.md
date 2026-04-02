# Architecture Overview

`gemini-live-transcribe` is a single-binary macOS terminal application for live transcription with Gemini Live. It captures one or more local audio sources, streams each source to its own Gemini Live session, and renders the resulting transcripts in a minimal ratatui interface.

## Domains

- Capture: local audio acquisition and normalization for `microphone` and `system-audio`
- Live session transport: Gemini Live WebSocket setup, audio upload, inbound event parsing, session resumption, and context-window compression
- TUI state and rendering: per-source transcript panes, status notices, controls, token/context display, and relative timestamps
- Local persistence: home-directory config resolution and JSONL session logging

## Layers And Dependency Direction

- `src/main.rs`: top-level composition root. Resolves configuration, starts capture workers and per-source transcriber supervisors, and runs the TUI event loop.
- `src/capture.rs`: source-specific audio capture. Converts local device or ScreenCaptureKit output into normalized mono `AudioChunk` values at `16 kHz`.
- `src/transcriber.rs`: Gemini Live boundary. Builds setup payloads, sends PCM audio, parses inbound server messages, and emits typed session events.
- `src/ui.rs`: ratatui surface. Owns `App`, `AppEvent`, per-source pane state, timestamp formatting, wrapping behavior, and token/context presentation.
- `src/session_log.rs`: JSONL audit trail for outbound setup/audio lifecycle events and inbound Gemini Live messages.
- `src/paths.rs`: canonical home-directory paths for config and logs.

Dependency direction is strictly inward:

1. capture/transcriber/session_log/paths do not depend on ui
2. `main.rs` is the only place that composes capture, transcriber, logging, and UI together
3. Gemini Live, cpal, ScreenCaptureKit, and terminal I/O are boundary integrations, not core state owners

## Cross-Cutting Concerns

- Source isolation: each selected source owns an independent capture path, Gemini Live session, transcript pane, and log file
- Failure visibility: permission issues, WebSocket failures, and capture errors should surface explicitly in the UI and logs
- Operator-first readability: transcript density, stable timestamps, and honest per-source status matter more than decorative structure
- Session durability: session resumption and context-window compression are enabled so long-running streams survive socket churn

## Entry Points

- `cargo run`: interactive app startup with optional prompts for API key and source selection
- `cargo run -- paths`: prints the resolved config and logs paths and exits
- `tests/paths_cli.rs`: CLI integration test for the `paths` subcommand
- unit tests in `src/*.rs`: behavior checks for capture batching, transcriber parsing, TUI layout, config resolution, and logging

## Related Docs

- [Frontend/TUI Surface](docs/FRONTEND.md)
- [Interaction And Visual Rules](docs/DESIGN.md)
- [Design Rationale Index](docs/design-docs/index.md)
- [Product Tradeoffs](docs/PRODUCT_SENSE.md)
- [Reliability Requirements](docs/RELIABILITY.md)
- [Security Requirements](docs/SECURITY.md)
