# Agent Focus Mode V2: working analysis

## Contract

Focus Mode is an explicit, per-workspace preference for this session. It remains enabled until explicitly toggled off. If enabled, the agent is visible, and every center pane is empty, the agent occupies the available center area. An occupied center pane, hidden/replaced agent, or zoomed view suspends the expanded layout without disabling the mode. Showing the agent after the center becomes empty expands it again; other docks and the threads sidebar remain independently usable. Focus must not be moved to an empty, hidden center by a panel focus toggle. No auto-expand setting or file-opening request tracking is needed.

## Slice 1: editor occupancy

- `Workspace::panes` tracks center panes; terminal-panel panes are distinct. `Pane::items_len()` counts all center items, including files, diff views and terminals, across splits. Counting all panes avoids treating a closed split as an empty workspace while another split contains an item.
- Pane add/remove events in `Workspace::handle_pane_event` are queued. The layout should derive occupancy from current pane state instead of interpreting an event as a permanent mode change. A redraw is needed when an item is added or removed.
- Existing `open_paths` and asynchronous editor entry points should stay unchanged; visibility derives from actual occupancy, not guesses about caller intent. This means background-created center items also occupy space, an inherent consequence of the requested state-based rule.

## Slice 2: dock geometry

- `Workspace::render` contains four existing bottom-dock trees; the center and side docks have different nesting in each. An expanded agent must fill the center column while preserving the opposite dock and bottom terminal, including contained layout. The agent's saved width/flex must not be overwritten by its expanded layout.
- `Workspace::render_dock` applies fixed or flexible dimensions to visible panels. An expanded agent requires an explicit fill rule; a proportional saved flex must not constrain its expanded width. Opposite dock resizing needs to use the expanded geometry rather than normal center proportions. Workspace resize during layout may require a deferred redraw for flexible opposite docks.
- A real UI pass on V1 exposed that `Dock::add_panel` reapplied `starts_open` during a move, making the focused agent disappear during Classic/Agentic swaps. V2 must test real settings-driven moves, both orders and both directions, not merely mutate panel positions synchronously.

## Slice 3: focus and visibility

- `Panel::is_agent_panel` already exists upstream; dock visibility and active panel are represented independently of workspace editor panes. This lets policy remain in workspace, without agent-specific types in the workspace crate.
- `Workspace::toggle_panel_focus` normally focuses the center after toggling an already-focused panel. In expanded mode that center is empty and hidden, so the action must not send focus into it; opening another dock must remain possible.
- Zoom is upstream's independent fullscreen layout. Enabling Focus Mode should not break zoom; whether zoom temporarily suppresses expansion can be derived rather than disabling the preference.
- Initialization/restoration must not persist the temporary expansion as a panel width or forcibly show a hidden agent. The explicit preference starts off for a new workspace unless a persistence requirement is added.

## Test matrix

- Enable/disable; empty and occupied center; agent hidden/reopened; explicit empty-editor focus toggle; closing last item; multiple splits and non-file items; hidden agent and last item closed; reopen with/without editor content.
- Both agent sides, real Classic/Agentic switches, same-dock replacement and opposite dock, fixed/flexible saved sizes, resizing handles, bottom-dock variants, sidebar, fullscreen and region focus.
- Background item add/remove, delayed opens and modal dialogs should remain normal upstream behavior; no bespoke asynchronous request plumbing.
- Compare diff against `origin/develop` for moved upstream blocks and avoid broad reformatting. GPUI tests should assert rendered geometry as well as state; verify actual UI in isolated Zed.
