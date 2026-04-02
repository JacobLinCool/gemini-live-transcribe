# Core Beliefs

- Minimize operator friction. The tool should be usable with one API key and a small set of explicit source selections.
- Keep sources independent. Each source owns its own capture path, Gemini Live session, and transcript pane.
- Prefer first-party capture paths. Use the native mechanism that matches the problem instead of forcing one abstraction to handle everything.
- Fail explicitly. Permission problems, malformed config, and server-side errors should surface clearly instead of hiding behind silent fallback logic.
- Bias the model before post-processing. Transcription preferences belong in Gemini Live instructions first, with UI normalization used only for obvious display cleanup.
- Keep the TUI legible under stress. The screen should privilege transcript density and current status over decorative structure.

## Extension Notes Worth Preserving

- Gemini Live inbound messages should be parsed as containers that can yield multiple internal events, because `usageMetadata` can accompany another server message type.
- Session resumption and context-window compression are part of the steady-state architecture for long-running sessions.
- Context/prompt token count is the operator-facing token metric that matters most for session pressure.
