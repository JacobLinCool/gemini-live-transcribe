# Security Requirements

This repository handles live audio, transcripts, and a Gemini API key. Treat it as a local desktop tool with real sensitive-data exposure, not as a toy demo.

## Trust Boundaries

- Local machine and terminal host: owns permissions, config, logs, and the running binary
- Gemini Live API: receives streamed audio and returns transcription/session events
- Home-directory storage: `~/.gemini-live-transcribe/config.toml` and `~/.gemini-live-transcribe/logs/`
- macOS capture frameworks: microphone input and Core Audio system-output taps controlled by OS permissions

## Sensitive Data Rules

- `api_key` is sensitive whether supplied by CLI flag, environment variable, or home config.
- Session logs contain transcripts and full inbound Gemini Live events. They are sensitive even though raw audio payloads are sanitized.
- Do not add fallback logging that stores raw audio blobs, secret-bearing headers, or unsanitized request payloads.

## Auth And Secret Management Expectations

- Prefer `GEMINI_API_KEY` or explicit `--token` for ephemeral use.
- Home config is supported for operator convenience, but it stores the API key in plaintext. Treat that file as user-owned sensitive material.
- Any change that broadens where secrets are read from or written to requires explicit review.

## Risky Change Review Triggers

- changes to session logging or sanitization rules
- changes to config resolution or secret-loading behavior
- changes to capture permissions, device selection, or source isolation
- changes that would transmit more local data to Gemini Live than the current audio/transcription flow

## Escalation Guidance

- If a proposed change weakens source isolation, stores more transcript data, or expands secret persistence, stop and review the trust-boundary impact first.
