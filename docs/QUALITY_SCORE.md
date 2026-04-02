# Quality Score

This document tracks agent legibility and repository truthfulness, not vanity metrics.

## Grades

### Runtime Architecture: B+

- Evidence: the repository has a clear single-binary shape with well-separated modules for capture, transcriber, UI, session logging, and home-path resolution.
- Evidence: unit tests cover the important local invariants for batching, event parsing, layout behavior, config generation, and log retention.
- Gap: there is still no committed end-to-end harness for a real Gemini Live session, so the highest-risk integration points are validated by local logic and manual runs rather than deterministic automation.

### Documentation Freshness: B

- Evidence: the old `docs/design.md` content has been re-homed into architecture, design-rationale, frontend, and product-sense documents that map to the live code.
- Evidence: `README.md` remains the human-facing run guide, while `AGENTS.md` now acts as the short agent entrypoint.
- Gap: there is no durable manual verification runbook for permission prompts, long-running session renewal, or transcript timestamp tuning.

### Reliability Legibility: B-

- Evidence: reliability-critical behaviors such as source isolation, JSONL audit logging, session resumption, and context-window compression are now documented.
- Gap: disk-pressure behavior, provider-side rate limits, and operator-visible recovery steps still rely on code inspection and ad hoc troubleshooting.

### Security Legibility: C+

- Evidence: the trust boundaries and sensitive-data rules are now explicit in `docs/SECURITY.md`.
- Gap: there is no automated guardrail for plaintext API keys in home config or for accidental retention of sensitive transcript logs beyond the current count-based retention policy.

## Doc Drift Treatment

- `README.md`: keep in place as the human-facing usage document, but treat it as an operational summary rather than the architecture source of truth.
- `docs/design.md`: replace and delete. Its responsibilities now live in `ARCHITECTURE.md`, `docs/design-docs/core-beliefs.md`, `docs/FRONTEND.md`, `docs/DESIGN.md`, and `docs/PRODUCT_SENSE.md`.

## Next Upgrades

- Add a durable manual verification runbook for live-session renewal, token/context display, and timestamp heuristics.
- Add a reproducible integration harness for Gemini Live that can exercise reconnect/resume behavior with recorded audio fixtures.
- Revisit the local audio-activity thresholds used for transcript segment start estimation and document tuned values after real-world runs.
