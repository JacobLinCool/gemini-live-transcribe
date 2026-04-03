# Reliability Requirements

The tool is intentionally small, but it still has a few hard operational invariants.

## Core Invariants

- Each source stays independent. `microphone` and `system-audio` must never share a Gemini Live session or a transcript pane.
- Capture failures should surface explicitly. Missing permissions or device errors are not hidden behind fallback capture paths.
- Session continuity should survive ordinary socket churn. Session resumption and context-window compression are part of the normal operating path, not a rare recovery mode.
- Session logs must remain reviewable after failures. JSONL logging is part of the debugging contract.

## Known Failure Modes

- Missing `HOME` breaks config/log path resolution.
- Missing Microphone permission blocks `microphone` capture.
- Missing System Audio Recording permission blocks `system-audio` capture.
- Missing default output device blocks `system-audio` capture.
- Gemini Live connection failures, `goAway` renewal, and close frames can interrupt one source while others keep running.
- Disk pressure can block build/test workflows and can also interfere with JSONL logging if the home volume is full.

## Recovery Guidance

- Run `cargo run -- paths` to confirm the active config and logs locations.
- Inspect the newest `~/.gemini-live-transcribe/logs/*.jsonl` file for the failing source before changing code.
- If only one source is failing, treat that source as the fault domain first; do not assume the whole app state is corrupt.
- When debugging timestamp or segmentation issues, inspect both the UI behavior and the corresponding session logs because the server transcript cadence and the local audio clock can diverge.

## Implementation Guardrails

- Prefer explicit notices in the TUI over silent retries that hide loss of continuity.
- Do not collapse sources into one shared transcript to paper over attribution or timing problems.
- Keep the operator-visible transcript truthful even if that means exposing brief reconnect states.
