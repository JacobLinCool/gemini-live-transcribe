# Tech Debt Tracker

## Open Items

### End-to-end Live API verification harness

- Why it matters: the highest-risk behaviors in this repo are provider-boundary concerns such as reconnect/resume, permission handling, and transcript timing under real audio.
- Desired future state: a reproducible integration harness or fixture-driven test workflow that can exercise Gemini Live session renewal and transcript timing without relying on ad hoc manual runs.

### Timestamp heuristic tuning

- Why it matters: transcript segment start time is estimated from local audio activity because Gemini Live does not return segment timestamps.
- Desired future state: measured thresholds and a documented tuning procedure for microphone and system-audio sources, ideally backed by representative recordings.

### Operational runbook for local failures

- Why it matters: the app depends on macOS permissions, home-directory storage, and a live provider connection, but the repo does not yet contain a single troubleshooting runbook.
- Desired future state: a compact operator/developer runbook covering `paths`, log inspection, common permission failures, and disk-pressure or reconnect diagnostics.
