# R5 — Extract a render model and overlay renderer

**Priority:** P1  
**Blocks:** R8  
**Blocked by:** R1

## Goal

Reduce `App::render` to layout preparation + one render call, while keeping the
existing ratatui output stable.

## Design

Create an owned `RenderModel` assembled before `Terminal::draw`. It should hold
sidebar entries, status tally, pane rectangles, footer hint, and overlay view
data. Move overlay drawing (Jump, Spawn, Help, Activity, Dashboard, Tasks,
Settings, custom command) into `overlays.rs` or equivalent pure rendering
functions.

Keep `Pane::render` and `sidebar::render_sidebar` as their existing seams.

## Acceptance criteria

- `App::render` is primarily layout/resize preparation and delegation.
- The draw closure does not read mutable `App` state through borrow-checker
  workarounds or clone ad hoc fields one overlay at a time.
- Existing render tests pass, including tiny terminal, many panes, sidebar,
  overlays, zoom, and viewport resize cases.
- Zoomed mouse hit-testing uses the focused pane index, not an implicit pane 0.
