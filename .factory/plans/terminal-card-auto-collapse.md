# Terminal card auto-collapse plan

## Objective

Add an Auto option to the agent terminal-card display setting. Auto should reuse the existing expanded and output-collapsed terminal behavior, then compact a finished command to a single `ToolTerminal` + `Run Command` disclosure row. Opening that row must reveal the existing full command and terminal output.

## Required behavior

| Setting and lifecycle | Command | Terminal output |
| --- | --- | --- |
| Always Expanded | Visible | Expanded |
| Always Collapsed | Visible | Collapsed |
| Auto, pending/running | Visible | Expanded |
| Auto, finished without a manual override | Replaced by compact `Run Command` row | Collapsed |
| Auto, finished and manually expanded | Visible | Expanded |

- Preserve the current meaning of the existing boolean modes. In particular, Always Collapsed must continue showing the full command and collapse only the terminal output.
- Auto is orchestration over the existing terminal card, not a parallel command or terminal renderer.
- A completed Auto card's disclosure expands and collapses the command and output together.
- While Auto is active, existing expanded-mode functionality must remain unchanged: command copy, terminal focus and scrolling, stop controls, status, elapsed time, failure state, truncation information, sandbox warning, permission controls, search, and background-terminal handling.
- Commands awaiting authorization must keep enough command detail visible for an informed decision.
- Preserve the existing thinking-block manual-override convention: Auto expands while active and compacts when finished if untouched; if the user collapses and explicitly re-expands it while active, that explicit expansion may remain open after completion.
- A terminal moved to the background should preserve its existing forced-collapse behavior and remain manually reopenable where supported.

## Settings design

1. Introduce a terminal-card display enum following the naming and derivation conventions of `ThinkingBlockDisplay`, with `Auto`, `AlwaysExpanded`, and `AlwaysCollapsed`.
2. Keep the public `agent.expand_terminal_card` JSON key for compatibility unless repository conventions provide a demonstrably safer migration.
3. Accept legacy booleans:
   - `true` maps to `AlwaysExpanded`.
   - `false` maps to `AlwaysCollapsed`.
4. Have the settings UI write string values and render the field as a dropdown titled `Terminal Card Display`.
5. Preserve the current default behavior as `AlwaysExpanded`; adding Auto must not silently change all users' existing terminal-card behavior.
6. Ensure the generated settings schema accepts both legacy booleans and the new string values so existing settings do not gain diagnostics.

## Implementation approach

1. Before modifying any source file, prepend the required two-line review notice to the root `README.md` exactly as required by `AGENTS.md`.
2. Trace all `expand_terminal_card` consumers and update settings content, resolved agent settings, defaults, settings UI registration/page data, and test fixtures coherently.
3. Keep generic tool-call/edit expansion state intact. Add terminal-specific state only where full-card Auto compaction cannot be represented by the existing output-expansion boolean.
4. Represent the three effective visual states clearly:
   - Full command with expanded output.
   - Full command with collapsed output.
   - Auto-compacted summary row.
5. Make lifecycle transitions derive from authoritative tool-call/terminal status or existing entry events. Do not mutate UI state during render.
6. Extend `TerminalToolHeader` rather than duplicating its status controls. In compact form, render the existing `ToolTerminal` icon, `Run Command` label, disclosure, and applicable status indicators; omit the command slot.
7. When compact Auto is reopened, route through the same command element and terminal view used by Always Expanded.
8. Keep the existing non-terminal preview/authorization path unchanged unless a test demonstrates that a terminal-backed authorization request needs an explicit visibility override.
9. Add or update the component preview to make compact, expanded, and output-collapsed states inspectable.

## Tests

1. Add settings-content tests for:
   - All string variants.
   - Legacy `true` and `false`.
   - Serialization to the canonical string representation.
   - Schema compatibility where practical.
2. Add focused state tests covering:
   - Auto pending/running equals Always Expanded behavior.
   - Untouched Auto compacts after success and failure/cancellation.
   - Opening a finished Auto card reveals command and output.
   - Always Collapsed continues showing the command while hiding output.
   - Manual collapse/re-expansion behavior while running.
   - Authorization and background-terminal edge cases.
3. Preserve and run existing terminal-card tests; do not replace them with Auto-only coverage.
4. Run formatting, focused crate tests, and a compile/check of every affected crate. Use `./script/clippy` rather than `cargo clippy` if lint validation is needed.

## Review checklist

- Auto contains no duplicate terminal rendering implementation.
- Existing Always Expanded and Always Collapsed behavior is unchanged.
- The compact row appears only for Auto lifecycle compaction.
- One disclosure from the compact row reveals both command and output.
- Existing boolean user settings continue to load without diagnostics.
- State changes refresh thread search and list measurement where required.
- No command approval can be presented without enough visible command context.
- Tests cover behavior equivalence as well as the new transition.
