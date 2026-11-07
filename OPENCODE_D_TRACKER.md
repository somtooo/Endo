# OpenCode D tracker

Everything still to do on OpenCode D, in one place. Update the status as work moves. Design background is in `OPENCODE_CLIENT_PLAN.md`.

**Goals:** first, every native Zed UI feature works with an OpenCode D thread. Second, native UI grows to support OpenCode's own features.

**Status:** `To do`, `In progress`, `Done`, `Needs check`, `Plan needed`, `Won't fix`.

**Source:** `You` means the user confirmed it. `Tester` means a QA run found it and the user hasn't confirmed it. `Code` means found by reading code.

## Bugs, in fix order

| # | Item | Details | Source | Status |
|---|---|---|---|---|

## Features to build

| # | Item | Details | Source | Status |
|---|---|---|---|---|

## Needs checking

Nothing waiting.

## Later

| # | Item | Details | Source | Status |
|---|---|---|---|---|
| 40 | MCP cleanup and per-agent MCP tools | Remove Zed's MCP servers from OpenCode when the last Zed thread for a folder closes, and choose MCP tools per OpenCode agent in the Agents modal. Not needed for now. | You | Later |

## Done or closed

| # | Item | Details | Source | Status |
|---|---|---|---|---|
| 39 | OpenCode D row in settings without OpenCode | The OpenCode D row on AI > External Agents now appears only when OpenCode is installed, or after installing it from an OpenCode D thread. Without OpenCode, the settings page looks the same as upstream Zed. Confirmed by the user. | You | Done |
| 37 | No sidebar spinner for a session already running when Zed starts | Reported by the user: a session running in the TUI showed no spinner in Zed's sidebar until its row was clicked. The agent couldn't reproduce it with a private service. A session started before Zed launched and one started after both showed an animated spinner. Confirmed working by the user. | You | Done |
| 1 | Agent edits missing from diff review | Finished edits now reach the action log, with the old text worked out by reversing OpenCode's patch. Keep, reject, new files, deletes, CRLF, and editing an old message all checked in the UI. Edits made through shell commands don't show, because OpenCode doesn't report them as file edits. | Tester | Done |
| 2 | Footer stuck after a failed compact | `/compact` on an empty thread fails, and "Waiting… /compact" stays in the footer even after later messages work. | You | Done |
| 3 | Stray `<system-reminder>` line | Plan-mode threads show a lone tag. Its body is missing from the stored thread too. | You | Done |
| 4 | Escape doesn't cancel a rename | Escape restores the title in the header and the sidebar, for every agent. Known limit: Escape on a thread with no title yet saves the default title as real, so it may not get a generated one. | You | Done |
| 5 | Rate-limit error shown twice | The turn's failure no longer repeats the failed step's error. Unit test only: a rate limit can't be triggered on demand. | Tester | Done |
| 6 | Agent-done notifications on Wayland | On Wayland, agent and terminal notifications go through D-Bus, for `primary_screen` and `all_screens`. User-tested on the desktop. | You, Code | Done |
| 7 | Profiles and the Build/Plan picker | OpenCode D threads use Zed's native profile picker (search, descriptions, Configure) listing OpenCode's primary agents. Configure, the panel menu's Profiles item and the Manage Profiles shortcut open an OpenCode Agents modal: fork, default model, tool permissions, delete. It writes OpenCode's global config, never Zed's profiles. The list refreshes on OpenCode's `agent.updated` event. User tested. | You | Done |
| 8 | Permissions | Per agent and for all agents, each tool is Allow, Ask or Deny in the OpenCode Agents modal, written to OpenCode's global config (`agent.<id>.permission`, `permission`). Ask uses the native permission prompt. The Tool Permissions settings page now says OpenCode D uses OpenCode's own permissions and where to set them. UI-tested by agents: Ask prompts, Deny refuses. Known limit, from OpenCode: denying Edit Files doesn't stop edits made through shell commands; deny Run Shell Commands too. User tested. | You | Done |
| 9 | Todo tool UI | OpenCode has a todo tool, but Zed has no native todo UI for any agent, so there is nothing to reach parity with. Tabled by the user. | You | Archived |
| 10 | Checkpoint restore | Not needed: upstream removed it for every agent (`eee7501970`, #65989). | Code | Won't fix |
| 11 | Allow and deny on permission prompts | Prompt appears; allow once, allow always and reject all behave correctly, checked on disk. | — | Done |
| 12 | Token fix: second long reply | Each long reply counted only itself, matching the service. | — | Done |
| 13 | Token fix: multi-step turn | Tokens added up across steps, matching the service. | — | Done |
| 14 | Thread following while streaming | Not a gap: Zed deliberately pins your message to the top while a reply streams (`ResponseAnchorPhase`), and OpenCode D uses the same view. | — | Won't fix |
| 15 | Model turn after a failed compact | OpenCode's service, not Zed: Zed only sends `POST /compact`, and `Session.compact` then wakes its execution loop. | Tester | Won't fix |
| 16 | `/goal` database-open error | User confirmed `/goal` works. | Tester | Done |
| 17 | Folder reads log errors | Zed tried to open a folder the agent read as if it were a file. OpenCode D skipped folders only when the output said "Read directory", but that's OpenCode's title, not its output. It now skips any path that is a folder on disk. Unit tested. UI-tested: no errors after reading a folder. | Tester | Done |
| 18 | Usage meter | A tester reported none, but screenshots show a circle beside the Build picker, probably the context meter. Likely a false report. User confirmed the meter works; the tester report was false. | You | Done |
| 19 | Turn stats tokens | Footer shows `4k tokens · 1m 14s`. Tokens arrive when a step finishes, because OpenCode doesn't report them while a step streams. In `crates/acp_thread` and `crates/opencode_agent/src/connection.rs`. | You, UI test | Done |
| 34 | Shell output shows twice | A short shell command showed its output twice. A finished foreground call has no shell ID, so the step that stops its output stream was skipped, and the stream added the output again. Fixed. Quick, streaming, failing and background commands each show their output once. UI-tested by the agent. | Tester | Done |
| 29 | Model picker Configure footer | OpenCode D's model picker shows Zed's Configure footer. It opens the settings page's Providers section, like the native picker opens LLM Providers. A newly connected provider's models appear in open threads without a restart. UI-tested by the agent. | You | Done |
| 30 | OpenCode D settings page | AI > External Agents has an OpenCode D row. Its page has: Providers (connect with an API key, sign-in or command; switch, rename and disconnect accounts; search all providers), Plugins (add, turn on or off, remove), Compaction, Warming, and Defaults (model, agent, web search). Writes OpenCode's global config and calls the service's API. UI-tested by the agent against a private service, with a fake API key and a Copilot sign-in that was started and cancelled, not completed. | You | Done |
| 31 | Background subagent management | Subagent cards already showed the work. Their Stop button now works, for background and foreground subagents, through OpenCode's interrupt. A stopped card shows Zed's native cancelled state. Limit: after restarting Zed, a stopped background subagent's card shows as completed, because only this run saw the interrupt. UI-tested by the agent. | You | Done |
| 25 | Steering queued messages | The native Steer button and Ctrl-Alt-S now work in OpenCode D threads. Steering hands the front queued message to OpenCode with `delivery: "steer"`, which adds it to the running turn at the next step. Undo, Stop, Clear All, delete and editing take it back (`DELETE inbox/:id`); OpenCode's `session.inbox.delivered` or `cancelled` event says which happened, so a late undo never sends it twice. Queued messages already worked. UI-tested by the agent: steer, undo, Stop, Send Now, edit, delete, and a steer at the end of a turn. | You | Done |
| 32 | Install OpenCode button untested | The install flow was tested on its own but never in the running app. The user will test it. | You | Done |
| 35 | Formatter and update settings | Not added to the settings page, by the user's choice. | You | Won't fix |
| 38 | Moved session stays under its old project | Moving a session to another folder left its sidebar row under the old project. OpenCode D now follows `session.moved`, and the startup sync corrects rows for sessions moved while Zed was closed. An open thread used to write its workspace's folder back over the move; it now keeps the session's folder when that's outside the workspace. UI-tested by the agent with git worktrees and the thread open: move, reply, and move back. A Zed restart wasn't UI-tested, because the test tool can't restart Zed with the same profile. | You | Done |
| 36 | Thinking blocks stay open | With thinking display set to Auto, a thinking block stayed open when another thinking block followed it, when a tool call followed it, or when the turn ended. All three now close it, unless you opened it yourself. This is a fix in shared `agent_ui` code, so it applies to every agent. Covered by a test that fails without the fix. | Plan | Done |
| 20 | Inline assistant and commit messages | Route to a Zed model. Left out on purpose. | You | Won't fix |
| 21 | Raw link text after backspace | Couldn't be reproduced. | You | Won't fix |
| 22 | Symbol picker shows nothing | Works for the user. The test run probably had no language server. | You | Won't fix |
| 23 | Image attachments | Work. | You | Done |
| 24 | Deleting a thread | Deletes the OpenCode session too. | You | Done |
| 26 | Split token display and limit warning | Not a gap: Zed shows one circle or two depending on the provider and model. One circle is fine. | You | Won't fix |
| 27 | Open as Markdown from the sidebar | Not useful enough. Comes for free if upstream Zed enables it for external agents. | You | Won't fix |
| 28 | Effort labels | Keep the names OpenCode's API returns; don't rename them to Zed's labels. | You | Won't fix |
| 33 | Clippy error in `install.rs` | `is_musl` used the blocking `std::process::Command::output`. It now runs `ldd` through `util::command::new_command` and awaits it. | Code | Done |

## Notifications

What item 6 has to do:

- Show a notification when the agent finishes or is waiting, including when the user is in another window or app.
- Respect `notify_when_agent_waiting`. Today `primary_screen`, the default, shows nothing on Wayland because `primary_display()` returns `None` (`crates/gpui_linux/src/linux/wayland/client.rs:1285`). `all_screens` works but draws a large box at the bottom of the screen.
- Clicking the notification brings Zed forward and opens that thread. The thread can be in another worktree, which changes the agent panel, so test that case.

Likely approach: GPUI already has `cx.show_system_notification`, which goes over D-Bus on Linux (`crates/gpui_linux/src/linux/system_notifications.rs`). Send that from `show_notification` in `crates/agent_ui/src/conversation_view.rs` instead of opening a popup window.

Upstream: #66287 (`upstream/agent-native-notifications`, unmerged) replaces the popups with system notifications, and #66285 (draft, async windows) pushes toward it. Drop our change when #66287 lands. Test runs have no D-Bus, so this needs testing on the real desktop.
