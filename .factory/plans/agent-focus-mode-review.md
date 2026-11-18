# Agent focus mode review log

The review sections preserve findings as originally reported. See “Implementation follow-up” for their current resolution and validation.

## Review 1 — 2026-11-18

Scope: commit `e3ba4d3f0a` against its original parent, `77f71f0f073cf461c2f20c822e56bbaf7f659533`. Use this pinned range rather than the moving `origin/main` reference.

Skills: `rust-review` and `rust-best-practices`. Reviewed as application code and internal infrastructure, not a published library API. Workspace edition: Rust 2024.

### Verdict

No must-fix correctness or safety issues were identified. One low-severity behavioral regression was identified. No implementation changes were made.

### Finding: width restrictions also affect unrelated panels in the expanded agent's dock (P3)

Locations:

- `crates/workspace/src/dock.rs:1504–1508`
- `crates/workspace/src/workspace.rs:2894–2896`

Both guards restrict the expanded dock rather than the expanded agent panel. Inactive non-agent panels sharing that dock lose their Flex Width / Fixed Width controls, although changing their sizing mode does not control the expanded agent's geometry.

Reproduction:

1. Put the terminal and agent in the right dock.
2. Show the agent with an empty center, close the bottom dock, and enable Focus Mode.
3. Right-click the inactive terminal's status-bar icon.
4. Observe that the terminal's width options are absent; suspend or disable Focus Mode and they return.

The right-click menu is built without activating the terminal first. Source verification included `TerminalPanel`'s flexible-sizing implementation and `RightClickMenu`'s mouse handler; no live UI reproduction was performed for this finding.

Rule: preserve behavior outside the intended change (`rust-best-practices`, §4, “Refactoring someone else's code”).

Suggested menu predicate:

```rust
!panel.is_agent_panel(cx)
    || workspace.expanded_agent_position(cx) != Some(dock_position)
```

Suggested guard in `toggle_dock_panel_flexible_size`:

```rust
if panel.is_agent_panel(cx)
    && self.expanded_agent_position(cx) == Some(position)
{
    return;
}
```

Suggested regression coverage: while the agent is expanded, an inactive non-agent panel in the same dock can still change sizing mode, while the expanded agent remains protected.

Status: open; not implemented.

### Positive observations

- Expansion derives from current occupancy, dock visibility, and zoom state rather than asynchronous request tracking.
- Expanded geometry does not overwrite the agent's ordinary saved width/flex state.
- Focus tests cover modals, background removals, split removals, editor landmarks, and fallback focus.
- Migration repair is transactional and covered by upgrade, on-disk reopening, and rollback tests.

### Validation record

- `git diff --check 77f71f0f07..e3ba4d3f0a`: passed.
- Earlier completed checks reported 18 focus-mode tests and 65 workspace persistence tests passing with Corgi `--force` (actual execution, not successful-test-result cache hits), plus 6 SQLez migration/savepoint tests and 5 DB tests.
- During the subsequent main-session continuation, fresh Corgi workspace tests, formatting, and direct Clippy attempts were blocked during dependency resolution because the branch's lockfile requires an update. The lockfile was left untouched, and a fresh full workspace result was not obtained.
- `CARGO=corgi ./script/clippy -p workspace --lib --locked` was separately blocked because the wrapper passes `--all-features`, which Corgi does not support.

## Review 2 — 2026-11-18

Independent subagent review of the same commit and parent. The reviewer received feature intent and review criteria, not Review 1's findings or conclusions, and was instructed not to read prior review reports or plan contents. It retrieved both Rust skills and independently identified the implementation diff. No implementation files were edited.

Additional design criteria: useless/organizational comments; shallow methods and unnecessary layers; non-descriptive names; functions not named as actions and types not named as domain nouns, with idiomatic Rust accessor/predicate exceptions; boolean blindness; duplicated policy; and scattered state/invariants. Findings had to explain a concrete behavioral or maintenance cost rather than request mechanical renaming.

### Accepted finding: duplicate workspace focus-target policy (P3)

Locations:

- `crates/workspace/src/workspace.rs:5059–5067`: `focus_visible_center_or_agent`
- `crates/workspace/src/workspace.rs:10351–10358`: `Focusable::focus_handle`

Both independently choose the expanded agent's activation focus handle, falling back to the active pane. Explicit focus handoffs and generic workspace fallback can diverge if only one implementation is changed later.

Rule: application-code simplicity and a single implementation of essential behavior (`rust-best-practices`, §1 and §4); the requested duplicated-policy/shallow-layer design review.

Suggested change: use the existing `Focusable` implementation at the helper's call sites and remove the duplicate helper:

```rust
self.focus_handle(cx).focus(window, cx);
```

Disposition: accepted as a small design improvement, not a current behavioral defect. Status: open; not implemented.

### Independently corroborated finding: dock-wide width restrictions (P3)

The reviewer independently identified Review 1's finding at `dock.rs:1504–1508`, `dock.rs:1529`, and `workspace.rs:2894–2896`: per-panel width preferences are restricted using a dock-wide expanded-agent condition. Its recommended fix matches Review 1: restrict only the expanded agent panel.

Disposition: corroborated; retain the existing open finding rather than count it twice.

### Unverified risk: focus after alternative dock-close routes

Locations:

- `crates/workspace/src/workspace.rs:4933–4939`: `close_active_dock`
- `crates/workspace/src/workspace.rs:2053–2063`: bottom-dock observer
- `crates/workspace/src/workspace.rs:2640–2645`: reconciliation requires center focus
- `crates/workspace/src/dock.rs:1423–1427`: closed dock retains its focus handle

The reviewer noted that `close_active_dock` closes the focused dock without explicitly selecting a visible focus target. When closing the bottom terminal lets the agent expand again, reconciliation may not help if focus remains in the closed dock rather than the center. GPUI's generic focus recovery may already handle this; source inspection alone did not settle it. The close path predates this feature.

Rule: visible-focus feature invariant and observable framework-specific test coverage (`rust-best-practices`, §11).

Required reproduction: with Focus Mode enabled, an empty center, and a visible agent, open/focus the bottom terminal, dispatch `workspace::CloseActiveDock`, pump frames, and assert focus is inside the expanded agent. Test `PanelEvent::Close` separately.

If reproduced, a possible fix after closing the active dock is:

```rust
if self.agent_focus_mode {
    self.focus_handle(cx).focus(window, cx);
}
```

Disposition: unverified integration risk, not a confirmed regression or an instruction to change code. Status: needs a runnable regression test.

### Not accepted as a new bug: opposite-dock fixed-width clamping

Locations: `crates/workspace/src/workspace.rs:10515–10535` and `crates/workspace/src/dock.rs:1280–1296`.

The reviewer classified the following as P2: with the agent expanded on the right, shrinking a 1200 px workspace to 700 px overwrites an opposite panel's 800 px fixed width with 694 px. It proposed skipping clamping on both side docks during expansion.

The mutation path supports the described behavior, but the parent commit already clamps both side docks on window bounds changes (`77f71f0f07`, `workspace.rs:10285–10300`). The feature deliberately exempts the expanded agent's dock while retaining normal opposite/bottom-dock behavior. The handoff also explicitly identifies preservation of all normal preferred sizes as a broader redesign outside this feature's scope.

The independent review brief's phrase “preserve normal saved panel widths ... through expansion and window resizing” can be read more broadly than that established scope. The reviewer's classification rests on that broader interpretation.

Disposition: existing normal-dock behavior, not an established feature regression. Do not implement the suggested change as a bug fix without explicitly expanding the sizing contract. An opposite-dock round-trip test could document the retained behavior.

### Naming and comments

The reviewer found no actionable comment or naming defects beyond the duplicate focus helper. The new comments explain non-obvious constraints, accessor/predicate names are idiomatic, and the persisted boolean appropriately represents an on/off preference. No mechanical verb/noun renaming is recommended.

### Additional test opportunities

- Alternative dock-close routes and final visible focus.
- Reopen an actual workspace and assert the restored preference plus expansion, beyond checking stored database values.
- Real `AgentPanel` activation-child focus, toolbar toggle state, and threads-sidebar interaction.
- Same-dock replacement and panel zoom/unzoom transitions with expansion and focus assertions.

These are coverage suggestions, not confirmed defects.

### Validation reported by the independent reviewer

- `corgi fmt -p workspace -p db -p sqlez -- --check`: passed in that pass.
- Formatting including `agent_ui`: failed on unchanged formatting in `agent_ui.rs` and `conversation_view.rs`.
- Focused Corgi tests with `--locked --force --timeout 60`: blocked before execution by the required lockfile update; no tests ran and no cached successful test result was used.
- `CARGO=corgi GITHUB_ACTIONS=true ./script/clippy --locked -p workspace -p agent_ui -p db -p sqlez`: blocked by the wrapper's unsupported `--all-features` argument.
- Feature diff whitespace check: passed.
- No tracked implementation/index changes; this review log was left untouched by the reviewer.

### Consolidated outcome

Two accepted P3 items: the existing dock-wide width restriction and the newly identified duplicate focus-target policy. One unverified focus risk requires runtime coverage. No additional blocking regression was established after checking the opposite-dock claim against the feature's original scope and parent implementation. No fixes had been applied at the time of this review.

## Implementation follow-up — 2026-11-18

### Resolved findings

- **Dock-wide width restrictions:** narrowed both the status-button menu predicate and `toggle_dock_panel_flexible_size` guard to the agent panel. Inactive non-agent panels in the same dock retain their width-mode controls, without changing the active panel or the agent's saved sizing state.
- **Duplicate focus-target policy:** removed `focus_visible_center_or_agent`; all of its call sites now use `Focusable::focus_handle` as the single target-selection policy.
- **Alternative dock-close focus risk:** reproduced as a failing GPUI test before fixing it. Dispatching `CloseActiveDock` while the bottom dock was focused failed to focus the expanded agent. Focus reconciliation now detects focus left inside a closed dock or its accessibility wrapper, as well as focus in a hidden center, and selects the visible workspace target. It runs through the existing dock observers, covering both `CloseActiveDock` and `PanelEvent::Close`. Reconciliation remains disabled outside Focus Mode and does not override active modals or focus in another visible panel. Renamed the method to `reconcile_focus_for_agent_mode` to describe its broader responsibility.

The inactive-panel width regression test also failed before the guard fix. Both regression tests pass after the changes.

### Added coverage

In `crates/workspace/src/workspace.rs`:

- Inactive same-dock panel width-mode changes, for both agent sides, while the expanded agent remains protected.
- Both alternative bottom-dock close routes, for both agent sides, restoring the agent's activation-child focus.
- Background dock closure preserving agent focus and active modal focus.
- Same-dock panel replacement and agent zoom/unzoom suspending and resuming expansion without disabling the preference.
- Reopening a real saved workspace through `open_workspace_by_id`, verifying expansion and persisted visibility for both visible and hidden agents.

In `crates/agent_ui/src/agent_panel.rs`:

- Actual Focus Mode toolbar clicks enabling/disabling the mode and changing rendered expansion.
- Enabling from the visible empty editor focusing the real agent thread's active input.
- Threads-sidebar opening/closing while Focus Mode stays enabled and the agent remains expanded.

### Final validation

All test commands used Corgi `--force`; test results were actually executed, not reused from the successful-test-result cache. Compilation artifacts were cached where available.

- `corgi test -p workspace --lib --locked --force --timeout 60`: **336 passed**.
- `ITERATIONS=20 corgi test -p workspace --lib agent_focus_mode --locked --force --timeout 60`: **23 passed across 20 seeds**.
- `ITERATIONS=20 corgi test -p agent_ui --lib '(test_agent_focus_mode_toolbar_and_threads_sidebar|test_clicking_tool_call_output_keeps_agent_panel_focused_and_zoomed|test_resubmitting_edited_message_keeps_zoomed_agent_panel_open)' --locked --force --timeout 60`: **3 passed across 20 seeds**.
- `corgi clippy -p workspace -p agent_ui --lib --locked -- -D warnings`: **passed**.
- `corgi clippy -p workspace -p agent_ui --all-targets --locked -- -D warnings`: blocked by existing `clippy::lint_groups_priority` errors in the `perf` package's lint configuration; not changed.
- `CARGO=corgi GITHUB_ACTIONS=true ./script/clippy --locked -p workspace -p agent_ui`: blocked by the wrapper's unsupported `--all-features` argument; no tooling changes made.
- `corgi fmt -p workspace -- --check`: **passed**.
- `corgi fmt -p agent_ui -- --check`: the changed `agent_panel.rs` is clean; the check reports only pre-existing differences in untouched `agent_ui.rs` and `conversation_view.rs`.
- `git diff --check`: **passed**.

To unblock execution, the original lockfile was backed up and Corgi was allowed to refresh it locally. The refresh added five missing registry packages and corrected stale dependency references. Validation above used that temporary refreshed lockfile. The original lockfile was restored afterward; no dependency changes are included in the patch.

Normal opposite-dock clamping remains unchanged, as previously scoped. No unrelated formatting, tooling, or lint issues were changed. No commits were created during the implementation follow-up.

## Commit consolidation and rebase — 2026-11-18

At the user's request, the implementation follow-up and this review log were folded into the existing feature commit, keeping the title `workspace: Add persistent agent focus mode`.

Fetched Endo `origin/main` and rebased the single feature commit onto `da82b47fb675f4d8ba0f859cc92ad6906a9023fa`. Because the base history had been rewritten, the rebase explicitly used the original parent `77f71f0f073cf461c2f20c822e56bbaf7f659533` as the old boundary to avoid replaying older fork commits. The rebase completed without conflicts. Before this log-only update, `git range-diff` reported the pre- and post-rebase feature patches as identical.

Validation repeated on the new base:

- Full workspace library suite: **336 passed**.
- Focus Mode workspace suite: **23 passed across 20 scheduler seeds**.
- Related Agent Panel toolbar/sidebar and focus/zoom tests: **3 passed across 20 scheduler seeds**.
- All tests were explicitly rerun with Corgi `--force`; successful-test-result caches were not reused.
- Targeted library Clippy with `-D warnings`: **passed using cached lint results** for unchanged source/dependencies.
- Formatting for `workspace`, `db`, and `sqlez`: **passed**.
- Feature diff whitespace check: **passed**.

The new base includes the existing lockfile repair, so all post-rebase validation used `--locked` without a temporary lockfile refresh. No dependency changes were added by the feature. The previously documented unrelated all-targets lint, wrapper, and `agent_ui`-wide formatting limitations remain outside this feature's scope.

The branch remains one feature commit ahead of the fetched `origin/main`. Nothing was pushed.
