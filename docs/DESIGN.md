# Design Rules

This repository has a terminal UI, not a dashboard product. The design rules should keep it legible under stress and honest about system state.

## Visual Direction

- Prioritize transcript density over decorative chrome.
- Keep one status block, one pane per source, and one compact controls block.
- Use color only for state meaning:
  - green: active capture/listening
  - yellow: reconnecting/resuming or in-progress draft text
  - red: error state
  - gray: inactive or neutral state

## Transcript Presentation

- Prefix each transcript row with `HH:MM:SS | ` using one shared relative zero point across all panes.
- Keep wrapped continuation lines aligned under transcript content, not under the timestamp prefix.
- Normalize only obviously awkward CJK spacing artifacts. Do not rewrite transcript content for style.

## Interaction Rules

- `q` should remain the single obvious quit control.
- Source status must be visible in the pane title.
- Context pressure should be visible in the pane title as `ctx`, because that is the operator-facing token number that matters for long-running sessions.
- Notices should remain short, current, and failure-oriented rather than trying to become a scrolling log viewer.

## Non-Goals

- no transcript editor inside the TUI
- no mixed-source transcript surface
- no decorative dashboards that bury failures under extra structure

## Related Docs

- [Frontend/TUI Surface](FRONTEND.md)
- [Core Beliefs](design-docs/core-beliefs.md)
