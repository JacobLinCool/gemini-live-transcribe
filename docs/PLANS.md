# Planning System

Execution plans live under `docs/exec-plans/`.

## Locations

- Active work: `docs/exec-plans/active/`
- Completed work: `docs/exec-plans/completed/`
- Durable documentation or implementation debt: [docs/exec-plans/tech-debt-tracker.md](exec-plans/tech-debt-tracker.md)

## Conventions

- Create one Markdown file per plan.
- Use short, descriptive slugs for filenames.
- Move plans from `active/` to `completed/` when the implementation and follow-up validation are done.
- If a plan reveals durable debt that will outlive the immediate task, copy that debt into the tech-debt tracker instead of leaving it buried in a completed plan.

## Current State

- There are no committed active execution plans yet.
- The tech-debt tracker is the canonical queue for known follow-up work that is not being executed in the current diff.
