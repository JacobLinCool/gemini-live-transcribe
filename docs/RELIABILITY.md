# Reliability Requirements

The tool is intentionally small, but it still has a few hard operational invariants.

## Core Invariants

- Each source stays independent. `microphone` and `system-audio` must never share a Gemini Live session, a finalizer queue, or a transcript pane.
- Capture failures should surface explicitly. Missing permissions or device errors are not hidden behind fallback capture paths.
- Session continuity should survive ordinary socket churn. Session resumption and context-window compression are part of the normal operating path, not a rare recovery mode.
- Session logs and transcript exports must remain reviewable after failures. JSONL logging and shutdown-time JSON export are part of the debugging contract.

## Known Failure Modes

- Missing OS application-directory metadata breaks config/log path resolution.
- Missing Microphone permission blocks `microphone` capture.
- Missing System Audio Recording permission blocks `system-audio` capture on macOS.
- Missing default output device blocks `system-audio` capture on macOS.
- Requesting an unsupported source on the current OS must fail before capture starts.
- Gemini Live connection failures, `goAway` renewal, close frames, or finalizer tool-call failures can interrupt one source while others keep running.
- Disk pressure can block build/test workflows and can also interfere with JSONL logging if the home volume is full.
- Slow finalizer throughput can build an in-memory pending-turn backlog and delay finalized transcript output.

## Recovery Guidance

- Run `cargo run -- paths` to confirm the active config, logs, and transcript-export locations.
- Inspect the newest JSONL log under the resolved logs directory for the failing source before changing code.
- If only one source is failing, treat that source as the fault domain first; do not assume the whole app state is corrupt.
- When debugging turn-boundary issues, inspect both the UI behavior and the corresponding session logs because Gemini Live AAD, provider transcript cadence, and the local audio clock can diverge.

## Implementation Guardrails

- Prefer explicit notices in the TUI over silent retries that hide loss of continuity.
- Do not collapse sources into one shared transcript to paper over attribution or timing problems.
- Keep the operator-visible transcript truthful even if that means exposing brief reconnect states or failed finalization markers.
