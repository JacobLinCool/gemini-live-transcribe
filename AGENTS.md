# AGENTS.md

This file is the agent entrypoint for `gemini-live-transcribe`. Read it first, then use the linked documents as the living sources of truth for architecture, UI behavior, operational constraints, and documentation debt.

## Reading Order

1. [Architecture Overview](ARCHITECTURE.md)
2. [Frontend/TUI Surface](docs/FRONTEND.md)
3. [Interaction And Visual Rules](docs/DESIGN.md)
4. [Design Rationale Index](docs/design-docs/index.md)
5. [Product Tradeoffs](docs/PRODUCT_SENSE.md)
6. [Planning System](docs/PLANS.md)
7. [Quality Ledger](docs/QUALITY_SCORE.md)
8. [Reliability Requirements](docs/RELIABILITY.md)
9. [Security Requirements](docs/SECURITY.md)
10. [Documentation Debt Tracker](docs/exec-plans/tech-debt-tracker.md)
11. [Reference Catalog](docs/references/index.md)

## Map Rules

- Keep this file short and navigational.
- Put durable detail in the linked documents, not here.
- Update this table of contents whenever a required or conditional knowledge-store doc is added or removed.

## Planning Rules

- When asked to write, revise, or extend an implementation plan, create or update a Markdown file under `docs/exec-plans/active/` instead of leaving the plan only in chat.
- Use [docs/PLANS.md](docs/PLANS.md) for execution-plan conventions and lifecycle rules.
- Treat the directory listing of `docs/exec-plans/active/` as the source of truth for current plans; do not encode the currently active plan name in `docs/PLANS.md`.
