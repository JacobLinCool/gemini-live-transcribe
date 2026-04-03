# Product Sense

This tool is built for an operator who wants live transcription with minimal setup and honest system behavior.

## User Framing

- Primary user: a terminal-native operator on macOS, Linux, or Windows who wants live text from local audio sources
- Primary workflow: start fast, choose sources explicitly, watch transcripts in real time, and inspect logs later if something goes wrong

## Prioritization Heuristics

- Prefer lower operator friction over extra configuration surfaces.
- Prefer truthful state over cosmetically smooth behavior.
- Prefer explicit failure notices over hidden fallback logic.
- Prefer stable per-source separation over clever transcript merging.

## Tradeoff Rules

- Bias finalized transcript behavior through the transcription profile and finalizer prompt, not through aggressive post-processing.
- Keep logs rich enough for offline debugging, but do not persist raw audio payloads.
- Treat source isolation as a product feature, not just an implementation detail.
- When long-running sessions require provider-specific machinery such as resumption or context compression, expose the resulting state honestly instead of pretending the connection is immortal.
- Prefer capability-based source exposure over platform marketing. If a source is not implemented on the current OS, do not present it as available.

## What Not To Optimize For

- backward compatibility with stale docs or old UI behavior
- full-fledged transcript editing
- hiding OS permission boundaries behind unsupported capture workarounds
