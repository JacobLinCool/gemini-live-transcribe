# Frontend Surface

The only UI surface in this repository is a terminal UI implemented with ratatui in `src/ui.rs`.

## UI Architecture Map

- `App`: top-level TUI state holder. Owns global start time, selected sources, per-source panes, and notices.
- `AppEvent`: the UI boundary type consumed by `App::apply`.
- `SourcePane`: per-source view model containing draft/finalizer status, queue metrics, turn lifecycle state, token/context snapshot, and cached layout state.
- terminal event thread: turns resize and quit keypresses into `AppEvent` messages without mixing terminal input logic into the async worker graph.

## Data Flow

1. `capture.rs` emits normalized `AudioChunk` values per source.
2. `main.rs` routes those chunks into one draft worker and one finalizer worker per source.
3. `transcriber.rs` converts Gemini Live server messages into typed events.
4. `main.rs` turns those typed events into `AppEvent` values and manages draft-to-pending-to-final turn transitions.
5. `ui.rs` renders the current app state into ratatui blocks and paragraphs.

## Important UI Boundaries

- The UI does not talk directly to Gemini Live or capture devices.
- Timestamp generation is derived from the local audio clock managed in `main.rs`, then rendered in `ui.rs`.
- The UI can normalize obvious CJK spacing artifacts, but it should not become a transcript rewriting layer.
- Draft text, pending turns, final text, and failed turns are distinct UI states and must not collapse back into one generic line type.

## Validation Expectations

- Multi-source panes should stay aligned under ordinary terminal sizes.
- Timestamp prefixes must remain stable within one utterance and comparable across panes.
- Draft, pending, final, and failed states should remain visually distinct.
- Resize handling must redraw correctly without corrupting cached layout state.

## Related Docs

- [Architecture Overview](../ARCHITECTURE.md)
- [Design Rules](DESIGN.md)
