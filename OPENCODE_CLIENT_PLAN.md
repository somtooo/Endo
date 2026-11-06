# Zed as a first-class OpenCode client

Status: built, on branch `feature/zed-client`. Open items are tracked in `OPENCODE_D_TRACKER.md`. Sections 1 to 14 are the original plan; notes marked **Now** say where the result differs.

## 1. Goal

Make OpenCode feel like the Zed agent inside Zed's agent panel, and fix what ACP breaks.

- **v1:** Zed talks to the OpenCode server directly instead of through ACP. Every agent panel feature that can work without UI changes works. Threads stay in sync with the server, and OpenCode sessions show up in Zed's sidebar automatically.
- **Later:** UI work for things OpenCode has and Zed lacks (background subagents, OpenCode agents as profiles), plus an OpenCode settings page. **Now:** all three are done (sections 6.2 and 15d).

**Constraint:** this fork merges from upstream Zed. Almost everything goes in a **new crate**. Changes to existing Zed code are kept small and listed in section 11.

## 2. The problem, with evidence

Today Zed runs `opencode acp`. ACP (Agent Client Protocol) is the generic protocol Zed uses for every external agent.

On 2026-11-01 this planning session ran in Zed and the OpenCode TUI at the same time:

- Zed's thread stopped at "Both explorers are still running…", with the spinner still going.
- The TUI showed the session kept going: both explorers finished, more turns ran, and the plan was written.
- Zed never received any of it, and gave no sign of whether the agent had stopped.
- Zed also showed "Run Command" blocks labelled "Map OpenCode V2 server API: …". Subagents ran those commands, but ACP merged them into the parent thread.

### Why it happens (confirmed in OpenCode's source)

Paths are in `~/devcave/opencode/packages/cli/src/acp/`.

1. **Private server.** `opencode acp` starts its own server tied to Zed's pipe (`commands/handlers/acp.ts:25`). If Zed closes, running work stops.
2. **Listens only during Zed's prompts.** Each prompt opens a fresh event subscription (`turn.ts:70-77`), which closes when the turn ends.
3. **Ignores turns Zed didn't start.** Events are dropped until Zed's own message is delivered (`translate.ts:126-128`).
4. **Background subagents start turns on the server.** When a subagent finishes, the server adds a completion message to the parent session, and the agent replies. No client sent that prompt, so the bridge never forwards it.
5. **Zed guesses busy/idle from its own prompts.** It never asks the server.
6. **Child sessions are flattened** into the parent thread.

### Background work that wakes the agent

OpenCode V2 has the same "wake up" model as Claude Code:

1. Work keeps running after the turn ends: a background shell command (`background: true`, `core/src/tool/plugin/shell.ts:55-57`) or a background subagent.
2. The agent's turn ends normally, and the session goes idle.
3. When the work finishes, a notice wakes the session, and the agent starts a new turn with no user prompt (`core/src/session/background.ts:8-11`, `core/src/session/subagent-completion.ts:21-45`).

Zed's thread can already show step 3. `AcpThread::update_session_state` (`acp_thread.rs:4703`) accepts agent-reported Running/Idle at any time, and tests cover an agent becoming active with no new prompt. The missing piece is the transport: the ACP v1 bridge never forwards step 3.

### Alternative considered: ACP v2

ACP v2 is a newer, **experimental** version of the protocol (`agent-client-protocol-schema-1.13.0/src/v2/mod.rs:3`). Zed's fork already has a full v2 client (`agent_servers/src/acp/v2.rs`). **OpenCode's bridge speaks only v1** (`cli/src/acp/service.ts:120`, `protocolVersion: 1`).

What v2 adds over v1:

- `StateUpdate`: running, idle or requires-action, reported by the agent at any time. This fixes the stuck spinner and the missed turns.
- `UserMessage` updates from the agent side, so prompts sent from the TUI can appear.
- `SessionInfoUpdate` (titles), `session/resume` with replay, `session/list`, `delete`, `fork`, `close`.
- Subagent child sessions routed to their own threads (Zed test `v2_subagent_updates_route_and_patch_child_sessions`).
- Prompt receipts: accepted is reported separately from finished.

What v2 does **not** have: rewind/truncate (edit previous message), retry, setting the title from the client, or anything that fills Zed's sidebar automatically.

**The ACP v2 route would mean:**

- Changing OpenCode's bridge (TypeScript, in the OpenCode repo):
  - speak v2
  - attach to the shared service instead of a private server (`commands/handlers/acp.ts:25`)
  - keep one event subscription per open session, not per prompt
- **Zed changes: almost none.** The v2 client already exists.

| | Direct HTTP client (this plan) | ACP v2 bridge |
|---|---|---|
| Missed turns, stuck spinner | fixed | fixed |
| Sessions survive Zed restart | yes | yes, if the bridge uses the shared service |
| Subagents as child threads | yes | yes |
| Edit previous message (rewind) | yes | **no** (not in v2) |
| Retry, rename synced | yes | partly (titles in, not out) |
| Automatic sidebar sync | yes (our crate) | **no**. Still manual import, or Zed code |
| Zed code changed | ~25 lines + new crate | ~0 |
| Where the work lives | a new Rust crate in this fork | OpenCode's repo: an upstream PR, or an OpenCode fork |
| Depends on | OpenCode's HTTP API (partly experimental) | the ACP v2 spec (experimental) in **both** Zed and OpenCode, plus OpenCode accepting the change |
| Helps other editors | no | yes |

**Decision:** direct client. Any protocol layer in between will always lag behind what OpenCode can do. ACP stays only as the fallback for remote projects.

**What we reuse from Zed's ACP v2 client.**

The biggest reuse is free. **`AcpThread`'s public API already takes ACP v2 types.** `upsert_user_message`, `upsert_assistant_message`, `upsert_thought`, `append_message_chunk`, `upsert_wire_tool_call`, `update_session_info`, `update_usage` and `update_session_state` all accept `acp_v2` structs from the `agent-client-protocol` crate. Zed's v2 dispatcher that calls them is about 70 lines (`InboundSessionNotification::apply`, `agent_servers/src/acp/session_updates.rs:271-345`).

So our crate's translation layer turns OpenCode events into ACP v2 structs and calls those methods. The structs are just data types; nothing goes over the ACP wire. Benefits:

- We use the same code paths Zed's v2 agents use, which are already tested: keyed user messages, streaming chunks, wire tool calls, state updates.
- We still call `AcpThread` directly for things v2 can't express, such as `rewind`.

The v2 *connection* code (`agent_servers/src/acp/v2.rs`) isn't visible to other crates: `mod v2;` isn't `pub` in `agent_servers/src/acp.rs:7`. We could make it `pub`, but we don't need it. It handles the ACP wire protocol, not our HTTP API. We copy these patterns from it:

1. **Accepted vs finished.** A prompt is "accepted" when the server takes it, and "finished" only when the agent reports idle (`V2SessionSubmissions`, `v2.rs:1894-1980`). OpenCode's `prompt` route works the same way.
2. **Stop waits for the agent.** Cancel keeps the prompt and waits for the agent's own idle report, instead of assuming idle (test `v2_cancel_sdk_preserves_receipt_and_waits_for_reported_idle`, `v2.rs:3082`). Our interrupt does the same with `session.execution.interrupted`.
3. **Updates that arrive early.** Updates that arrive before a response are held and applied in order (`ordered_response`, `v2.rs:1135-1270`). This is the same problem as "events arrive while the message snapshot loads" (section 10).
4. **Subagents.** Child sessions become their own `agent_owned` threads, and replayed child state is marked as history after a resume (`v2.rs:1056-1060`, test `v2_subagent_updates_route_and_patch_child_sessions`).
5. **Session list.** Paging and filters (`V2SessionList`, `v2.rs:1719-1790`).

## 3. Decisions made

1. **OpenCode owns file writes.** Zed's keep/reject review UI stays off for this backend, as it is over ACP today. Edit tool cards still show a read-only diff.
2. **Only sent prompts sync between clients.** Unsent text stays local to each client, as in the OpenCode TUI and web app.
3. **Clients are programs, not people.** A permission request lives on the server. Whichever client answers it, every client gets `permission.replied` and closes its prompt.
4. **Multi-folder workspaces already work.** OpenCode uses the main folder as its location. Zed records the thread under every folder in the workspace. No work needed.
5. **New crate first.** Changes to `agent_ui` come later, and stay in this fork. They must be clean and isolated so upstream merges stay easy.
6. **Use the shared OpenCode service** (explained in section 5.1).
7. **Rules:** OpenCode reads the project's `AGENTS.md` itself. Zed sends nothing extra. **Now:** a global "OpenCode instructions" setting isn't planned; OpenCode reads `~/.config/opencode/AGENTS.md` itself.
8. **MCP:** forward Zed's MCP servers the same way ACP does today, side effects included (section 5.7). Kept simple on purpose for v1.
9. **Remote (SSH/WSL) projects keep using ACP.**
10. **API churn:** treat experimental OpenCode routes as unstable. Use only what we need, ignore unknown events, and check the server version.
11. **Settings UI is out of scope.** OpenCode has many config locations, so an "Edit opencode.json" button needs its own design. Later. **Now:** built as a page on AI > External Agents that edits OpenCode's global config and calls the service (section 9a).
12. **Fast models stay in the model dropdown.** In OpenCode's catalog, "GPT-5.5 Fast" is its own model, not a variant, so it appears in the model dropdown next to "GPT-5.5". The effort dropdown ("Medium") holds the model's variants.
13. **Same option IDs and values as OpenCode's ACP, and favorites keep working.** Our dropdowns use the IDs `model`, `effort` and `mode`. Model values look like `claude-agent/opus` (`cli/src/acp/config-option.ts:22-66`). Favorites and saved defaults work through the existing `CustomAgentServer` code, stored under `agent_servers."opencode-direct"` in `settings.json` (`agent_servers/src/custom.rs:80-125, 163-190`). On first use, our crate copies your existing ACP OpenCode favorites and defaults (`agent_servers.opencode`) into that entry once, so they carry over. After that the two lanes keep separate lists.
14. **A separate, always-present "OpenCode D" entry.** The existing ACP "OpenCode" entry stays exactly as it is, untouched and in its own lane, for upstream compatibility. OpenCode D (direct) is built in, like the Zed agent: no install step, always in the agent picker. The two never share code paths, settings or agent IDs (section 4.5).
15. **Use the system OpenCode, never Zed's ACP download.** OpenCode D connects to the running service. If none is running, it starts one with the `opencode` binary on your system. If there's no system binary, it installs one to OpenCode's standard location (section 5.2). Zed starts the service at startup when a system binary exists (section 9a).
16. **v1 changes no UI.** The native thinking control comes in the first UI change after v1 (section 9b). **Now:** the UI changes since v1 are listed in sections 11 and 15d.
17. **Running sessions go to the sidebar, the rest to history** (section 14).

## 4. How Zed works

All paths are relative to `crates/`.

### 4.1 One thread UI for every agent

The panel renders one thread type, `AcpThread` (`acp_thread/src/acp_thread.rs`), in one view, `ThreadView`. The native Zed agent and ACP agents use the same path. The native agent is a bridge that drives `AcpThread` from its own events (`agent/src/agent.rs:891, 2780, 1822`).

The ACP adapters in `agent_servers` drive `AcpThread` using only its **public** API (`agent_servers/src/acp/session_updates.rs`). So a separate crate can drive a thread fully.

### 4.2 What the UI unlocks per backend

**Optional hooks on `AgentConnection`** (`acp_thread/src/connection.rs:95-316`). Our crate can fill any of these:

| Hook | Unlocks |
|---|---|
| `receipt_submissions` (:220) | Agent-reported busy/idle. **Required** for `update_session_state` (`acp_thread.rs:4290, 4703`). |
| `truncate` (:264) | Edit previous message (rewind) |
| `retry` / `supports_retry` | Retry button |
| `set_title` (:272) | Renaming sends the new title back |
| `session_config_options` (:303) | Dropdowns and toggles: model, effort, mode, booleans. If present, the dedicated model and mode pickers are not built (`conversation_view.rs:1646-1682`). |
| `session_list` (:311) | Listing and deleting sessions. Used by thread import. |
| `load_session` (:122) | Reopening a thread with history |
| `request_elicitations` (:260) | Forms |

**Config options render generically.** Claude's ACP agent gets its **Fast** toggle this way: a boolean config option with category `ModelConfig` (`agent_ui/src/config_options.rs:1666`; your `settings.json` stores `"fast": false` for `claude-acp`). No native-only code is involved. Our crate can show any OpenCode setting the same way.

**About 45 native-only checks in `agent_ui`** downcast to `NativeAgentConnection` (`thread_view.rs:1786-1798`, `conversation_view.rs:3398-3412`). They gate the profile selector, steering, `@thread`, `@skill`, the split token display, and title regeneration. These wait for later.

### 4.3 Driving a thread from our crate

All public on `AcpThread`:

- **User messages:** `upsert_local_user_message(ClientUserMessageId, content)` (:7198). The message gets a client ID, which makes it editable.
- **Assistant output:** `append_message_chunk` (:5286), `upsert_assistant_message` (:5065), `upsert_thought` (:5079).
- **Messages between sessions** (subagent completion): `upsert_session_message` (:5093).
- **Tool calls:** `upsert_tool_call` (:6212), `update_tool_call` (:6114), `resolve_locations` (:6503, drives "follow agent").
- **Plans / todos:** `update_plan` (:7049).
- **Usage:** `update_usage` (:6093), `update_token_usage` (:6077).
- **Title:** `update_session_info` (:6012).
- **Busy/idle:** `update_session_state` (:4703), `clear_session_state` (:4795).
- **Permissions:**
  - ask: `request_tool_call_update_authorization` (:6703)
  - close one answered by another client: `authorize_tool_call` (:6815), `cancel_permission_request` (:6793)
- **Forms:** ask with `request_elicitation` (:6940). Close with `respond_to_elicitation` (:6976) or `cancel_elicitation` (:7013).
- **Compaction:** `push_context_compaction` (:5874), `update_context_compaction` (:5978).
- **Slash commands:** `update_available_commands` (:4110).
- **Config changes from the server:** feed a `ConfigOptionUpdate` through `handle_session_update` (:4873).
- **Subagents:** `register_subagent` (:6060), `.agent_owned()` (:4048).

**Gap: removing entries.** `truncate_entries` is private (:5856). The only public removal is `rewind(client_id)` (:7723). It calls our `truncate` hook, removes everything from that user message on, and calls `action_log.reject_all_edits`. Our edit log is empty, so that last step does nothing.

**Workaround (no Zed change):** every user message gets a client ID. When the server reverts, or after a reconnect, call `rewind` on the first message that differs. Our `truncate` hook skips the server call when the server started the removal. Then re-append from the server's messages.

**Gap: client IDs from strings.** `ClientUserMessageId` only creates UUIDs (`connection.rs:18-28`). It derives `Deserialize`, so `serde_json::from_value(json!("msg_…"))` builds one from OpenCode's ID. No Zed change.

### 4.4 Thread history and import

- The sidebar and history read one store, `ThreadMetadataStore` (`agent_ui/src/thread_metadata_store.rs`). Each row holds a thread ID, session ID, agent ID, title, folder paths, timestamps and an archived flag.
- **Import today** (`agent_ui/src/thread_import.rs`):
  1. Connect to the agent and call `session_list` (:689-792).
  2. Page through all sessions (:794-820).
  3. Skip sessions already in the store, and sessions without folder paths.
  4. Write a row per session with `archived: true` (:855-888).
  5. Save with `ThreadMetadataStore::save_all` (:383).
- Opening a row later calls `load_session` with the stored session ID. Rows are placed by their folder paths, which is why imported threads land under the right project.
- **The store's API is public:** `global`, `entry_by_session`, `save`, `save_all`, `archive`, `unarchive`, `set_generated_title`. So our crate can import automatically, and keep rows up to date, without changing Zed code.

### 4.5 Registering the backend

There is no extension point today:

- `Agent::server()` (`agent_ui/src/agent_ui.rs:580-593`) hard-codes the native agent and `CustomAgentServer`.
- `CustomAgentServer::connect` (`agent_servers/src/custom.rs:198`) always starts an ACP process.
- The agent store's list (`AgentServerStore::external_agents`, `project/src/agent_server_store.rs:189`) is rebuilt only from `agent_servers` settings on every settings change (`reregister_agents`, :295-420).

**Key fact: one list drives all the UI.** These all read `AgentServerStore::external_agents`:

- the picker's "External Agents" section, with labels and icons (`agent_panel.rs:6014-6064`)
- which connections stay alive (`agent_connection_store.rs:316-321`)
- history and archive restore, via `Agent::Custom { id }` → `CustomAgentServer` (`agent_ui.rs:535-546, 587`)

So if OpenCode D is in that list, it appears everywhere **without changing `agent_ui`**.

**Agent ID:** `opencode-direct`. **Label:** "OpenCode D". It never touches the ACP `opencode` entry.

**Hooks:**

1. **Built-in agents in the store** (`project/src/agent_server_store.rs`, ~25 lines). A global list of built-in agents, each with an ID, display name, icon and an `ExternalAgentServer` stub. `reregister_agents` re-inserts them after rebuilding from settings, for local projects only. It also ignores any `agent_servers` settings entry with a built-in's ID. Such an entry appears when you star a model (`custom.rs:92-97` writes a registry-type entry), and it should only hold favorites and defaults. Our crate adds OpenCode D at startup.
2. **Connection factories** (`agent_servers`, ~25 lines). A global registry of connection factories keyed by agent ID. At the top of `CustomAgentServer::connect`, if a factory is registered for this ID, use it. Otherwise ACP as today. Our crate registers one for `opencode-direct`. Every other agent, including ACP OpenCode, is unaffected.
3. **Hide built-ins from Settings → External Agents** (`settings_ui/src/pages/external_agents_page.rs:79-96`, ~3 lines). That page lists every agent in the store, and would give OpenCode D configure and remove buttons that do nothing, because it isn't in `settings.json`. Skip built-ins there. Optional: without it, the row is just inert.

**Remote projects:** built-ins are local-only, so OpenCode D doesn't appear for SSH/WSL projects. The ACP "OpenCode" still does, if you installed it.

### 4.6 Rewind: Zed vs OpenCode

- **Zed's rewind** (editing a previous message) cancels the turn, deletes that message and everything after it (`agent/src/thread.rs:2494`), and undoes agent edits you haven't accepted (`acp_thread.rs:7759`). It never touches accepted edits.
- **OpenCode's revert** also restores every file changed from that message onward, from its own snapshot.

With OpenCode, editing a message in Zed behaves like the TUI. That is intended.

## 5. How OpenCode V2 works

Source: `~/devcave/opencode/packages/`. Docs: <https://opencode.ai/v2/docs/api>. Installed: v2.2.5.

### 5.1 The shared service

OpenCode V2 runs **one background server per user account**. The TUI, web app and desktop app don't run the agent themselves. They connect to that one server, which runs every session. That's why a session started in the TUI keeps running when you close the TUI, and why the web app can show it.

- **"Shared service"** means Zed connects to that same server. Zed sees the same sessions as the TUI, live, and they survive Zed restarts.
- **"Private server"** means Zed starts its own OpenCode server just for itself. This is what ACP does today. Its sessions die with Zed, and the TUI sees its work only after the fact.

Decision: shared.

**Finding it:** `~/.local/state/opencode/service.json` (mode 0600) holds `{id, version, url, pid, password}` (`cli/src/services/service-registration.ts:14-46`). Read it, probe `GET /api/info`, then connect with HTTP Basic auth (username `opencode`).

**Starting it** when none is running: `opencode serve --service`. That is what the official clients do (`client/src/effect/service.ts:49-127`).

**Version mismatch:** OpenCode's own client *replaces* a running service whose version differs (`client/src/effect/service.ts:45-47`, `service-probe.ts:22-24`). The CLI, by default, refuses instead (`cli/src/services/server-connection.ts:80-88`). **Zed must never replace a running service.** Two clients with different OpenCode versions would keep killing each other's server, and with it every running session. Zed reuses whatever is running if its API version is supported, and starts one only when none is running.

### 5.2 Getting the binary (system OpenCode, not Zed's ACP download)

The ACP "OpenCode" entry downloads its own copy into Zed's data folder (`~/.local/share/zed/external_agents/registry/opencode/v_<version>_…/opencode`). **OpenCode D never uses that copy.** The service is shared with the TUI and desktop app, so it should run from the same OpenCode install they use.

Order, when OpenCode D connects:

1. **A service is already running** (found through `service.json`, healthy on `/api/info`): connect. No binary needed. This is the normal case. On this machine the service runs from `~/.opencode/bin/opencode`.
2. **No service, system binary found:** run `opencode serve --service`, then connect. Zed looks for the binary:
   - on `PATH`, using the project's shell environment, as Zed does for other tools
   - then in `~/.opencode/bin/opencode`, where OpenCode's installer puts it (`install:69`, `INSTALL_DIR=$HOME/.opencode/bin`)
3. **No system binary:** install OpenCode to `~/.opencode/bin`, the official location, so the TUI and Zed share it. Then go to step 2. How it installs, and whether it asks first, is decision 1 in section 14.

Zed never updates or replaces a system OpenCode, and never replaces a running service (section 5.1).

### 5.3 Main routes

| Area | Routes |
|---|---|
| Sessions | `GET/POST /api/session`, `GET/PATCH/DELETE /api/session/:id`, `GET /api/session/active`, `POST :id/fork` |
| Input | `POST :id/prompt`, `POST :id/command`, `POST :id/compact`, `POST /api/experimental/session/:id/skill` |
| Control | `POST :id/interrupt`, `POST :id/agent`, `POST :id/model` |
| Revert | `POST :id/revert/stage {messageID}`, `DELETE :id/revert`, `POST :id/revert/commit` |
| Read | `GET :id/message` (paged), `GET :id/diff` |
| Permissions | `GET :id/permission`, `POST :id/permission/:rid/reply {decision, message?}` |
| Forms | `GET :id/form`, `POST :id/form/:fid/reply {answer}` |
| Catalog | `GET /api/agent`, `/api/model`, `/api/command`, `/api/skill` |
| MCP | `PUT/DELETE /api/experimental/mcp/:server` |
| Events | `GET /api/event` |

**Prompt body:** `{text, files?, agents?, skills?, id?, delivery?, resume?, metadata?}`.

- `id` lets Zed choose the message ID. Format: `msg_`, then 12 hex characters (timestamp × 0x1000 + counter), then 14 random base62 characters (`schema/src/identifier.ts`). It is time-ordered.
- The model and agent are session state, set with `:id/model` and `:id/agent`.
- Effort levels are **model variants** (`provider/model#variant`). "Fast" models are separate catalog entries, not variants.
- `files[].uri` accepts `file://` and `data:` URIs, so pasted images work (`core/src/session/prompt.ts:110`).

### 5.4 How the TUI and web app handle events

Both use the same code (`client/src/solid/connection.ts`, `client/src/solid/data.ts`):

- **One global stream with no filter.** The server can only filter by event type. They take every event and route it themselves.
- Events are batched every 10 ms. Reconnect after 1 s. A stream with no bytes for 45 s counts as dead.
- **If a client falls behind**, the server drops its connection after 4096 queued events. The client just reconnects.
- **They never replay.** On reconnect they re-fetch active sessions, then each open session's newest messages, permissions and forms.
- **Opening mid-turn:** text deltas are live-only. Text that streamed before you opened the session appears when that text block ends.

Zed copies this design exactly.

### 5.5 Messages, revert, subagents

- **Messages:** `GET :id/message` returns a flat list of `user`, `synthetic`, `assistant` (one per step, with text, reasoning and tool items), `shell`, `compaction`, `agent-switched`, `model-switched` and `idle` (end of turn) items.
- **Revert:**
  1. `revert/stage {messageID}` restores files from the snapshot and hides the later messages.
  2. The next prompt commits the revert.
  3. The TUI flow is interrupt, wait, stage, then refill the composer.
- **Subagents:** each one is a child session with `parentID`. A background child returns right away. When it finishes, the server adds a completion message to the parent, which starts a new parent turn (`core/src/session/subagent-completion.ts:21-45`).

### 5.6 Rules and skills

- **Rules:** OpenCode loads **only `AGENTS.md`**: its own global one (`~/.config/opencode/AGENTS.md`), and every `AGENTS.md` from the project folder up to the project root (`core/src/config/plugin/instruction.ts`). It does not load `CLAUDE.md` or `.rules`.
- **Zed today** has no separate "rules library" any more. It was migrated into skills plus a global `AGENTS.md` (`prompt_store/src/rules_to_skills_migration.rs`). Zed has:
  - a global `~/.config/zed/AGENTS.md`
  - per project, the first file found from `.rules`, `.cursorrules`, …, `AGENTS.md`, `CLAUDE.md` (`prompt_store/src/prompts.rs:22-32`)
- **v1:** nothing extra. OpenCode reads the project's `AGENTS.md`.
- **Not planned:** a setting for global OpenCode instructions (decision 7).
- **Skills:** OpenCode scans `~/.agents/skills` and every `.agents/skills` folder from the project upward (`core/src/config/plugin/compatibility.ts:45`, `discovery.ts:152-166`). **These are the same folders Zed uses** (`agent_skills/agent_skills.rs`). OpenCode already sees Zed's skills. In v1 they appear as `/` commands from `GET /api/skill`.

### 5.7 MCP

**What the TUI does:** nothing special. It uses only the MCP servers in `opencode.json`. It never adds any.

**What Zed adds:** Zed has its own MCP list (`context_servers` in Zed settings). Over ACP today, Zed sends that list to OpenCode (`agent_servers/src/acp/v1.rs:9845-9880`), and OpenCode's bridge registers each server with `PUT /api/experimental/mcp/:name` (`cli/src/acp/sessions.ts:93-120`). That route:

- applies to the **whole project folder**, so TUI sessions in that folder see Zed's servers too
- lives **in memory**, so it disappears when the service restarts or the folder is idle for 60 minutes
- **replaces** a server in `opencode.json` that has the same name
- is never removed by the ACP bridge

That is the "side effect": it's the cost of pushing Zed's list into a shared server. The TUI never pushes anything, so it never hits this.

**v1:** do exactly what ACP does, for parity. Also re-register after a reconnect, so servers survive a service restart.

**Later:** cleanup when the last Zed thread for a folder closes, and per-agent MCP tool selection (Zed's profile "Configure MCP Tools" maps to OpenCode agent permissions). Tracker #40, not needed for now. A `zed-` prefix isn't needed: Zed never sends a name OpenCode's own config already has, and only removes names it added.

### 5.8 Config

- OpenCode config is per project. Layers, lowest to highest: managed policies, global `~/.config/opencode/opencode.json(c)`, project `opencode.json(c)` walking up, `.opencode/` folders, env vars (`core/src/config.ts:363-373`).
- Clients can't change most config over the API. `PATCH /api/experimental/config` only changes `shell`.
- Compaction is config-only: `compaction: {auto, buffer, keep}`.

## 6. v1 scope

### 6.1 In v1

| Feature | How |
|---|---|
| Send and stream reply | `prompt` + event stream |
| Turns started elsewhere (subagent completion, TUI, web) | apply every event for the session |
| Busy/idle spinner | `session.execution.*` → `update_session_state` |
| Stop | `interrupt` |
| Edit previous message (rewind) | client IDs + `truncate` → interrupt → `revert/stage` |
| Retry | `retry` hook |
| Rename, synced both ways | `set_title` → `PATCH`; `session.renamed` → `update_session_info` |
| Model dropdown, including "Fast" models, with favorites | config option `model` from `GET /api/model`, same values as OpenCode's ACP |
| Effort dropdown | config option `effort` (ThoughtLevel) from the model's variants |
| Agent dropdown (Build, Plan, custom agents) | config option (Mode) from `GET /api/agent` |
| Tool approvals, synced across clients, with reject feedback | `permission.asked` / `replied` |
| Forms (questions) | `form.created` / `replied` / `cancelled` |
| Slash commands, including skills | `GET /api/command`, `GET /api/skill` |
| `/compact` | `POST :id/compact` |
| Message queue | Zed's local queue. It sends when the server reports idle |
| `@file`, `@directory`, `@symbol`, `@selection`, `@fetch`, `@diagnostics`, branch diff, image paste | files as URIs; the rest as text (section 8.1) |
| Tool cards, with read-only diffs for edits | OpenCode tool → Zed card kind; patch content |
| Follow agent | tool locations from tool inputs |
| Plan / todos | `todowrite` → `update_plan` |
| Token usage ring | `session.usage.updated` → `update_usage` |
| Subagents as separate cards that open their own thread | `register_subagent` |
| **Automatic thread sync into history**, even if you never use the OpenCode agent | section 9 |
| Reopen any thread | `load_session` |
| Open as Markdown | same as ACP today. The sidebar shows it for any thread loaded in the panel (`sidebar.rs:6490`, `is_live`), whatever the agent. No work needed |
| Notifications, sound | generic thread events |
| MCP forwarding (ACP parity) | section 5.7 |
| Remote projects | OpenCode D isn't offered; use the ACP "OpenCode" entry |

### 6.2 Later

| Feature | Status |
|---|---|
| Native thinking control (lightbulb + effort) | Done (15d) |
| Profile selector → OpenCode agents | Done: the native picker lists OpenCode's primary agents, and Configure opens the OpenCode Agents modal |
| Steering queued messages | Done: `delivery: "steer"`, and clicking Steer again takes the message back |
| `@thread` | Done (15d) |
| `@skill` mention | Done (15d) |
| Split token display, token-limit callout | Won't fix: Zed shows one circle or two depending on the provider and model |
| Background subagent management UI | Done: Stop on subagent cards, with Zed's cancelled state |
| Open as Markdown for threads not loaded in the panel | Won't fix: not useful enough |
| OpenCode settings page | Done (section 9a) |
| Global OpenCode instructions setting | Not planned (decision 7) |
| MCP cleanup and per-agent MCP tools | Later, tracker #40 |
| Native model picker | Done (15d), with the Configure footer opening the settings page's Providers section |
| Native message-box layout | Done (15d) |
| Live output for background shell commands | Done (15d). Foreground shells stream too, and a quick shell's output shows once |

### 6.3 Not planned

- Thumbs-up/down feedback. It reports to Zed's telemetry, which isn't useful for OpenCode.
- Shared (collab) projects. Only the Zed agent supports them.
- Keep/reject review (decision 1).

### 6.4 Zed settings and OpenCode

| Zed setting | Effect on OpenCode | Where to configure OpenCode instead |
|---|---|---|
| `context_servers` (MCP) | forwarded (section 5.7) | — |
| Skills folders | the same folders, read by OpenCode | — |
| `auto_compact` (enabled, threshold) | none | `compaction` in `opencode.json`, or the OpenCode D settings page |
| `default_model`, `favorite_models` | none (favourites work for dropdown values) | `model` in `opencode.json`, or the OpenCode D settings page |
| `profiles` | none | `agents` in `opencode.json`, or the OpenCode Agents modal |
| `tool_permissions` | none (also not applied over ACP today) | `permissions` in `opencode.json`, or the OpenCode Agents modal |
| `compaction_model`, `thread_summary_model`, `subagent_model`, `model_parameters` | none | `agents.*` in `opencode.json` |
| Global `~/.config/zed/AGENTS.md` | none | `~/.config/opencode/AGENTS.md` |
| Notifications, sound, layout, card expansion, `use_modifier_to_send` | yes (generic UI) | — |


## 7. Crate layout

New crate `crates/opencode_agent`. The name `opencode` is taken by the OpenCode Zen model provider.

- **`client`:** discovery, auth, starting the service, HTTP calls, the event stream. Uses Zed's `http_client`.
- **`types`:** hand-written serde types for the routes and events we use. Unknown event types are ignored.
- **`connection`:** `OpenCodeConnection`, which implements `AgentConnection`, plus the factory registration.
- **`translate`:** Zed → OpenCode (prompts, mentions) and OpenCode → Zed (events → `AcpThread` calls).
- **`sync`:** per-thread state, resync after reconnect, rewind-based rebuilds.
- **`history`:** automatic import into `ThreadMetadataStore` (section 9).

## 8. Translation layer

### 8.1 Zed → OpenCode

| Zed action | OpenCode call |
|---|---|
| New thread | `POST /api/session {location}`, then forward MCP servers for the folder |
| Send message | generate a `msg_…` ID, convert mentions, `POST :id/prompt {id, text, files}` |
| Slash command | `POST :id/command {name, text}`, or `…/skill` for a skill |
| `/compact` | `POST :id/compact` |
| Stop | `POST :id/interrupt` |
| Edit message N | interrupt → wait for idle → `revert/stage {messageID: N}`. The next prompt commits |
| Model / effort | `POST :id/model {providerID, id, variant}` |
| Agent | `POST :id/agent {agent}` |
| Approve / reject | `POST :id/permission/:rid/reply {decision, message?}` |
| Answer form | `POST :id/form/:fid/reply {answer}` |
| Rename | `PATCH :id {title}` |
| Delete | `DELETE :id` |
| Open old thread | `GET :id` + `GET :id/message` |

**Mentions:**

| Zed mention | Sent as |
|---|---|
| file, directory | `files: [{uri: "file://…", mention: {start, end, text}}]` |
| image | `files[]` with a `data:` URI |
| symbol, selection, terminal selection, merge conflict | text block with path and line range |
| diagnostics, fetch, branch diff | text block (Zed already resolves these) |

### 8.2 OpenCode → Zed

| OpenCode event | `AcpThread` call |
|---|---|
| `session.execution.started` / `succeeded` / `failed` / `interrupted` | `update_session_state`; error notice on failure |
| user message (from message sync) | `upsert_local_user_message`, with the `msg_…` ID as client ID |
| `session.text.delta` / `ended` | `append_message_chunk`; on `ended`, `upsert_assistant_message` with the full text |
| `session.reasoning.*` | `append_message_chunk(Thought)` / `upsert_thought` |
| `session.tool.*` | `upsert_tool_call` / `update_tool_call` |
| `session.created` with `parentID`, or a `subagent` tool | `register_subagent` |
| subagent completion message | `upsert_session_message` |
| `permission.asked` / `replied` | request / `authorize_tool_call` / `cancel_permission_request` |
| `form.created` / `replied` / `cancelled` | `request_elicitation` / `respond_to_elicitation` / `cancel_elicitation` |
| `session.usage.updated` | `update_usage` |
| `session.renamed` | `update_session_info`, plus the sidebar title |
| `session.moved` | the sidebar row moves to the session's new folder |
| `session.agent.selected` / `model.selected` | config-option update |
| `session.compaction.*` | compaction entries |
| `session.retry.scheduled` | `update_retry_status` |
| `session.revert.staged` / `committed` | rewind from the first hidden message |
| `command.updated`, `skill.updated`, `agent.updated`, `model.updated`, `provider.updated`, `integration.updated`, `credential.updated` | re-fetch, then update commands, options and the model list |
| stream lost | `handle_connection_lost`, reconnect, resync |

## 9. Automatic thread sync (replaces manual import)

Today you open thread history and click **Import Threads**. v1 does the same work automatically, using the same store and the same row format. So threads land under the right project exactly as imported ones do.

**It does not depend on the agent panel.** Import today connects through the agent (`thread_import.rs:689-712`), so it only runs when you ask. Our sync instead starts from the crate's `init` at Zed startup. It talks to the OpenCode service directly over HTTP and writes rows straight into `ThreadMetadataStore`, whose API is public (section 4.4). So if you only ever use the Zed agent, restart Zed, and search your history, OpenCode sessions from the last few days are there.

It needs the OpenCode service running, because the sessions live in OpenCode's database and the service is the only way to read them. If it isn't running, Zed starts it (decision 15, section 9a). It only runs when OpenCode is added in Zed's agent list.

1. **At startup** (once the workspace is restored), and for each open local project folder: `GET /api/session?parentID=null&directory=<folder>`, paged. Top-level sessions only. Subagents stay inside their parent. Also when a new project folder is opened.
2. For each session with **no row** in `ThreadMetadataStore` (`entry_by_session`), write one: agent `opencode-direct` (so reopening uses OpenCode D), title, folder paths from the session's location, timestamps, and `archived: true`, as import does.
3. **Live updates from events:**
   - `session.created` (top-level) → add a row
   - `session.renamed` → update the title
   - `session.moved` → move the row to the new folder. An open thread keeps the session's folder when it's outside the workspace it's open in
   - activity → update `updated_at`
   - `session.archived` / `unarchived` → archive or unarchive the row
   - `session.deleted` → remove the row
4. **Reconnect** → repeat step 1 to fill any gap.

Rows Zed already has, for threads created in Zed, are left as they are. That keeps multi-folder placement.

## 9a. Installing, removing, and the settings page

**Two separate lanes:**

| | "OpenCode" (ACP) | "OpenCode D" (direct) |
|---|---|---|
| Where it comes from | Settings → External Agents, from the ACP registry | built in, always in the picker |
| Agent ID | `opencode` | `opencode-direct` |
| Binary | Zed's own download, run with `acp` | the system `opencode` (section 5.2) |
| Server | private, dies with Zed | the shared service |
| Install / remove | the External Agents page, unchanged | nothing to install or remove |
| Remote projects | yes | no |
| Code | upstream Zed, untouched | our crate |

Because the lanes share nothing, installing or removing ACP OpenCode never affects OpenCode D, and the reverse. That removes the settings clash.

**What OpenCode D does in each situation:**

| Situation | Zed startup | Clicking OpenCode D |
|---|---|---|
| Service running | sync history | connect |
| No service, system `opencode` found | start the service, then sync | (already started) connect |
| No system `opencode` | nothing. No install at startup, no sync | install OpenCode (decision 1, section 14), start the service, connect, sync |
| You uninstall system OpenCode | if a service is still running, sync still works. Otherwise nothing | same as "no system `opencode`" |

Details:

- **Zed never installs anything at startup.** Installing only happens when you pick OpenCode D. Startup only starts a service that's already installed.
- **Zed never stops the service.** The TUI or desktop app may be using it.
- **Synced history rows** use the ID `opencode-direct`, so they reopen through OpenCode D. Sessions that already have a row (for example, ones you made through ACP OpenCode, ID `opencode`) are skipped. The check is by session ID, as import does (`thread_import.rs:867`). So nothing appears twice.
- **ACP OpenCode threads** keep reopening through ACP. OpenCode D doesn't take them over.
- **Turning OpenCode D off** (if you want a machine with OpenCode installed but no Zed sync): a hidden setting, later if needed. Not built.

**The settings page (built).** AI > External Agents has an OpenCode D row, shown only when OpenCode is installed or after installing it from an OpenCode D thread. Its page has Providers (connect by API key, sign-in or command; switch, rename and disconnect accounts; search), Plugins (add, turn on or off, remove), Compaction, Warming, and Defaults (model, agent, web search). It writes OpenCode's global config with V2 keys, keeping comments, and calls the service for connections and plugins. Terminal-only settings and those Zed already has (appearance, diffs, alerts, terminal, debug, MCP) are left out on purpose, and so are the formatter and update settings.

## 9b. The native thinking control (lightbulb + effort)

In the Zed agent's message editor, the lightbulb toggles thinking. When thinking is on, an effort dropdown ("max") appears next to it. With OpenCode over ACP, there's no lightbulb. Instead there's a model dropdown, an effort dropdown ("Medium") and an agent dropdown ("Build").

The native control is built by `render_thinking_control` (`agent_ui/src/conversation_view/thread_view.rs:6616-6700`). It only runs for the native agent: it reads `as_native_thread` and asks the model for its effort levels.

- If the model can't turn thinking off at any effort level, it shows **only the effort dropdown**, no lightbulb (:6637-6648).
- Otherwise it shows the lightbulb, and the effort dropdown while thinking is on.

**OpenCode mapping:**

- Effort levels = the current model's variants (`Model.Info.variants`).
- Turning thinking off: OpenCode has no thinking flag, only variants. If a model has a variant that turns reasoning off (e.g. `none`), the lightbulb maps to it. Otherwise the native code's own rule applies: show the effort dropdown only, no lightbulb.

**Done (15d).** The plan was: a small trait on the connection, e.g. `AgentThinkingControls`, with `effort_levels`, `selected_effort`, `set_effort`, `can_disable_thinking`, `thinking_enabled`, `set_thinking_enabled`. `render_thinking_control` asks the trait instead of `as_native_thread`. The native agent and OpenCode both implement it. When OpenCode provides it, our crate stops sending the separate `effort` dropdown, so the effort control isn't shown twice. The model and agent dropdowns stay as they are.

## 9c. Archiving and the sidebar spinner

**What "archived" does.** It's a flag on the thread's row (`ThreadMetadata.archived`, `agent_ui/src/thread_metadata_store.rs:325`).

- Writing rows with `save` / `save_all`, as import does, has **no side effects**: no git work, no session calls (`thread_metadata_store.rs:691-701`).
- `store.archive` flips the flag and tells the panel to drop a background view of that thread (`agent_panel.rs:1543-1551`). If the connection says so, that view's session is closed.
- The **heavy** part is only the sidebar's "Archive Thread" action (`sidebar.rs:5334`). It can commit and remove git worktrees that Zed created for the thread (`thread_worktree_archive.rs:218-328`). Our sync never calls it.
- **Opening an archived row un-archives it** (`agent_panel.rs:4532-4536`).
- The sidebar **never** shows archived rows (`thread_metadata_store.rs:633, 655`). History shows all rows, is searchable, and has no running indicator.
- Nothing auto-archives old threads.

**Where the spinner comes from.** A sidebar row shows a spinner only if an agent panel holds a loaded view of that thread (`sidebar.rs:8100-8158`, from `AgentPanel::conversation_views()`). `ThreadMetadata` has no status field, and nothing else can feed one. So after a restart, a running OpenCode session in the sidebar shows no spinner until you open it.

**Done.** The plan was: A small global status store in `agent_ui`, keyed by session ID. Our crate updates it from `session.execution.*` events. The sidebar reads it for rows that aren't loaded (`sidebar.rs:1792-1800`, plus the project header's running count at `:1780-1788`). About 30 lines across `agent_ui` and `sidebar`.

Rejected alternative: keep running sessions loaded in the panel in the background, so they count as "live". That needs a new public panel API (`insert_retained_thread` is private, `agent_panel.rs:4277`). Background views aren't restored after a restart (`:1184-1265`), and they only exist for open workspaces.

## 10. Keeping open threads in sync

Copies the TUI and web app (section 5.4):

1. **One event stream** for the whole server, owned by one connection object. Parse and batch on a background thread. Drop events for sessions with no open thread before they reach the main thread.
2. **Opening a thread:** start buffering events, fetch the session and messages, build the entries, then apply the buffered events, skipping any already covered.
3. **Live:** apply every event for the session, whoever caused it.
4. **Reconnect** (including after falling behind): re-fetch active sessions, then for each open thread its newest messages, permissions and forms. Where Zed's entries differ, rewind to the first differing user message and re-append.
5. **Revert from another client:** same as step 4.
6. **Opening mid-turn:** the current text block fills in when it ends, as in the TUI.

## 11. Changes to existing Zed code

| Change | Size | Required? |
|---|---|---|
| Built-in agents in the agent store, re-inserted by `reregister_agents` (`project/src/agent_server_store.rs`) | ~25 lines | Yes |
| Connection-factory registry, and a check in `CustomAgentServer::connect` (`agent_servers`) | ~25 lines | Yes |
| Skip built-in agents on Settings → External Agents (`settings_ui/src/pages/external_agents_page.rs:79-96`) | ~3 lines | Optional. Without it, OpenCode D shows there with buttons that do nothing |
| Add the crate to the workspace and call its `init` (`Cargo.toml`, `zed/src/main.rs`) | a few lines | Yes |
| `pub` truncate or replace entries (`acp_thread`) | ~5 lines | No. Only if the rewind workaround proves fragile |

**Now:** the shared changes made since v1. Each one defaults to the native behavior, so other agents are unchanged.

| Change | Where |
|---|---|
| Hooks that let OpenCode D supply skills, `@` thread text, the thinking control, profiles, steering and a session's folder | `agent_ui/src/agent_menu_providers.rs`, plus a one-line fallback at each native call site |
| The model picker's Configure footer can run another action | `footer_action` in `acp_thread/src/connection.rs`, used in `agent_ui/src/model_selector.rs` |
| An open thread keeps its session's folder when the session moved out of the workspace | `agent_ui/src/thread_metadata_store.rs` |
| Thinking blocks close when the reply moves on (a fix for every agent) | `agent_ui/src/entry_view_state.rs`, `conversation_view.rs` |
| The OpenCode D settings page and its row on External Agents | `settings_ui/src/pages/opencode_*.rs`, `external_agents_page.rs` |
| The settings path the Configure footer opens | `zed_actions/src/lib.rs` |

## 12. Existing bug found: thinking blocks that stay open

Not caused by this project, but it shows up in OpenCode threads today, and the direct client will hit it too.

With `thinking_display: "auto"`, Zed opens a thinking block while it streams, and should close it when the reply moves on. The logic is `EntryViewState::auto_expand_streaming_thought` (`agent_ui/src/entry_view_state.rs:227-266`). It tracks **one** auto-opened block at a time:

- When a **new** thinking block starts, it opens the new one and starts tracking it (:248-253). **It never closes the previous one.** So when two thinking blocks follow each other with nothing in between, the first stays open for good.
- It only closes a block when the last entry is something other than a thought (:254-262). It is called on entry updates (`conversation_view.rs:2040`), but **not** when a new entry is added (`conversation_view.rs:1996-2023`). So if a tool call appears as a new entry right after a thought, the close step doesn't run then.
- When the turn ends, `clear_auto_expand_tracking` (`conversation_view.rs:2112`, `entry_view_state.rs:219-221`) forgets the tracked block **without closing it**. A block still open at that moment stays open.

OpenCode makes all three more likely. It sends each reasoning part as its own message (`cli/src/acp/translate.ts:196-198`, `reasoningMessageID`), so thinking blocks often come back to back, and often end a turn.

**Fixed** (`agent_ui: Close automatically opened thinking blocks when the reply moves on`, tracker #36). The plan was: when switching to a new block, close the previous one unless you opened it yourself. When clearing the tracking at the end of a turn, close the tracked block under the same rule. Also run the check on new entries. This touches `agent_ui`, so it's a separate small fix, and a good candidate to send upstream.

## 13. Risks

1. **Sync correctness (hardest).** Reconciling the message snapshot with live events, and rebuilding through `rewind`, is the riskiest code. The spike must prove it.
2. **Rewinding before the first user message.** The workaround can't remove entries that come before the first user message. Rare. Fallback: the small `acp_thread` change in section 11.
3. **Version mismatch with the running service.** OpenCode D uses the system binary, so normally Zed and the TUI share one version. If the running service's API is newer or older than our crate supports, Zed shows a clear error and never replaces the service (section 5.1).
4. **API churn.** Experimental routes may change. Use few, ignore unknown events, check the server version, and test our types against `protocol/openapi.json`.
5. **Unsaved buffers.** OpenCode reads and writes the disk, so it can't see unsaved Zed edits. Same as ACP today.

(Event volume is not a risk. The TUI and web app read the same unfiltered stream. It only means Zed must parse off the main thread, as section 10 says.)

## 14. Open decisions

None right now. Resolved:

- **Installing OpenCode** when you pick OpenCode D and none is on the system: ask first, then install. The thread shows "OpenCode isn't installed" with an Install button. Zed downloads the official release for your platform, checks its checksum, and puts it in `~/.opencode/bin`. Zed never runs the `curl … | bash` script.
- **The native thinking control** comes after v1. v1 changes no UI. **Now:** done.
- **Running sessions show in the sidebar; the rest go to history** (replaces decision 17). Sync writes sessions that are running with `archived: false`, so they appear in the sidebar under their project, and everything else with `archived: true`. A session that starts running later is un-archived. Writing the flag has no side effects (section 9c).
- **The sidebar spinner after a restart** needs a small UI change, so it comes with the first UI change after v1 (section 9c). In v1, a running session shows in the sidebar without a spinner until you open it; once open, it streams live. **Now:** done. Zed asks the service which sessions are running at startup and marks their rows. Tracker #37 holds one report of a missing spinner that couldn't be reproduced.

## 15. Phases

1. **Spike: client and sync, no UI. Done.** See section 15a.
2. **v1 backend. Done.** See section 15b.
3. **Later:** section 6.2. Done, apart from tracker #40.

## 15b. v1 status

Branch `feature/zed-client`, rebased onto `develop`. Commits:

- `opencode_agent: Add OpenCode server client and session sync spike`
- `opencode_agent: Add OpenCode D, a direct client of the OpenCode service`
- `opencode_agent: Show forms, forward MCP servers, and match the native option order`
- `opencode_agent: Offer to install OpenCode when it's missing`

Built, and tested by hand: live sync with the TUI, reverts, the sidebar spinner and titles, history sync, the icon, native edit diffs (including relative paths and empty files), subagent cards (background, continue, no cancelling), shell terminal cards, the background shell spinner, the token circle, and the option order.

Also tested by hand: forms (the question tool), MCP forwarding, and Open as Markdown. Extension MCP servers such as GitHub's run without a `context_servers` settings entry, so the list comes from `server_ids()` filtered by `is_server_enabled()`, not `configured_server_ids()`. Each server sent is logged to `Zed.log` as `Registered MCP server <name> for <folder>`.

Install: with no `opencode` on PATH or in `~/.opencode/bin`, OpenCode D shows Zed's sign-in prompt with an **Install OpenCode** button. It downloads the latest npm package for this platform, checks its SHA-512 hash, and writes `~/.opencode/bin/opencode`. Tested by the user in the running app.

## 15c. Slash and @ menus (done, see 15d)

Found after v1 testing. Skills from `/api/skill` aren't wired into the `/` menu (only `init`, `review`, `goal` and `compact` show), and the popups look plainer than the native agent's. Cause: these are native-only in `agent_ui`.

| Feature | Native-only because | Fix |
|---|---|---|
| `/` Skills section and `@` skills | one skill list, filled only by downcasting to `NativeAgentConnection` (`conversation_view.rs:1571`, `:2314`) | an `AgentConnection::available_skills` hook; fixes both menus |
| `compact` icon | `completion_provider.rs` shows `IconName::Compact` only for category `Native` named `compact` | mark our `compact` with `meta_with_command_category(Native)`. Native commands are sent bare and the rest is queued (`send_command_queueing_remainder`), which suits `compact` only; `init`/`review`/`goal` take arguments, so leave them unmarked |
| `@` threads | thread store passed only for the native agent (`agent_panel.rs:4674`); list filtered to `ZED_AGENT_ID` (`completion_provider.rs:2436`); content from the native agent's LLM summary (`mention_set.rs`, `confirm_mention_for_thread`) | hooks to list a connection's mentionable threads and give a thread's contents. OpenCode has no session reference in prompts, so OpenCode D sends a plain transcript built from the session's messages |
| OpenCode built-in skills (`opencode`, `report`, `browser`) | choosing a skill reads its file (`mention_set.rs:500`); these have no file | list them as plain `/` commands |

Sending skills: like the TUI, the prompt carries `skills: [{id, mention?}]` (`schema/src/prompt-input.ts`). `review` and `goal` are both commands and skills; the command wins in `/`.

**Decided: a registry, not trait methods.** A new file in `agent_ui` holds slots that `opencode_agent` fills at startup, like `register_agent_connection_factory`. Each native-only call site keeps its code and gains a one-line fallback that asks the registry (for example `.or_else(|| extra_skills(&connection, cx))`). New files never conflict on upstream merges, so each existing site changes by about one line. Trait methods with defaults are the cleaner form if this is ever proposed upstream.

Earlier recommendation, superseded: the two hooks, with defaults that change nothing for ACP agents. About 6 upstream files: `acp_thread/connection.rs`, the native agent connection, `conversation_view.rs`, `agent_panel.rs`, `completion_provider.rs`, `mention_set.rs`. Same "connection capabilities" approach as the native UI work in 6.2. **Alternative with no upstream changes:** skills as plain `/` commands, no `@` skills or threads.

Retry: not implemented, and OpenCode has no retry route (the TUI has none either; the server retries provider errors). Drop it from 6.1.

## 15d. Native UI parity (goal: match the native agent wherever possible)

Any visible difference from the native agent is a bug: fix it, or list it here with why it waits.

Fixed (all user-tested, commit `2d5bca0fd3`):
- `/` menu: one "Commands" section (external agents' own commands share the native group in `completion_provider.rs`); `compact` icon; Skills section with built-in / global / project labels.
- `@` menu: Skills and Threads; thread chips open in the thread's own agent. A mentioned thread is summarized by OpenCode (`POST :id/generate`, which leaves its history alone) with the native summary prompt; the chip pulses while it loads, as native. The summary uses the mentioned thread's model, since the route takes no model; native uses its cheaper summary model.
- Compaction: native "Context Compacted" divider via `upsert_context_compaction_update`, not an assistant message.
- Commands no longer leave a "Waiting…" bar: their receipt is the latest user message.
- Sent messages keep their chips (files, selections, skills, threads, images). Zed's display blocks go in the prompt's `metadata.zed.content` (OpenCode's web app keeps its own presentation the same way) and redraws rebuild from them. Before, OpenCode's echo of the message replaced it with plain text.

Open:
- Old threads with messages sent from Zed before the metadata change show those messages as plain text with file chips at the end. Won't fix: those threads are being deleted.

Fixed later:
- Messages from the TUI or web app get chips where they were typed. Like the TUI's `realignPromptMentions`, a mention is found by its text (longest first); its recorded position only picks among repeated copies, since the TUI counts terminal columns. Line ranges (`?start=&end=`) become selection chips, `application/x-directory` files folder chips; unknown skills stay text.
- Live shell output: running shells (foreground via progress metadata, background until their notice) are polled with `GET /api/shell/:id` and `/output?cursor=` each second and appended to the display terminal. A streamed terminal keeps its output when the call ends (OpenCode's result is capped), with any error shown under it. A shell that ends before anything streamed shows its final output as before. Long logs replay their last 1 MB; characters cut between pages are completed on the next read.
- Native model picker: OpenCode D implements `AgentModelSelector`, so the native picker shows its models with provider icons, groups named by provider (`/api/provider`), token prices (`in$/out$`; free models show none) and favorites (stored where the old dropdown kept them, `favorite_config_option_values.model`). `conversation_view.rs` builds the picker when an agent's config options have no model option; only OpenCode D does both.
- Native thinking control: a thinking provider in `agent_menu_providers.rs` feeds the native lightbulb and effort selector (`thread_view.rs` gains a fallback line at each native hook). Every model gets the lightbulb beside the effort selector. Off hides the selector and, for models with a `none` variant, switches to it; models without one keep their effort, since OpenCode cannot stop them thinking. On restores the effort from before. Effort levels are the variants plus "Default" (no variant).
- Saved defaults now apply: a new thread takes the model, effort and agent last picked in Zed (`default_config_options`). Before, they were saved but never read, so new threads used OpenCode's default model.
- Not matched: the picker has no Recommended section (OpenCode has no such list).
- Configure footer: done. It opens the settings page's Providers section rather than the native provider settings (`footer_action` in `acp_thread`).
- Placeholder reads "Message OpenCode D — @ to include context, / for commands"; the native one says "Message the Zed Agent, …". The wording is chosen per agent in `conversation_view.rs` (`placeholder_text`).

Found by the Cua test pass (isolated compositor, fresh profiles, Nov 4), fixed and verified in the UI:
- `/` menu showed only `compact` in a folder the service hadn't loaded yet. The first commands and skills request for a cold directory returns empty lists; the service then sends `command.updated` and `skill.updated`. The hub now refetches on those events (and after a reconnect), and a version count catches events that arrive while a thread is still loading. Command names are kept per session; one shared set was overwritten by the last thread opened, so `/review` could go out as text in another folder's thread.
- Edit cards showed "Edited src/main.py (1 replacement)" instead of a diff when the model passed a relative path: the patch header is relative too. Relative paths now resolve against the session's directory. The same fix gives relative reads and edits a location, so "Go to File" and Follow work for them.
- Directories are no longer tool locations (list, glob, grep, and reads of a directory); Zed tried to open them as files.
- Stopping a turn showed "Error: Step interrupted". That error is hidden, as in the TUI; other aborts (a turn that can't be resumed) still show.
- Every opened thread creates a session, so untitled sessions nobody prompted filled history as "New Agent Thread" (27 of the last 200 here). History sync skips them and removes the rows earlier syncs added (archived, not renamed). Sessions stay on the server.

Verified with no change needed: model picker (search, groups, costs, check), thinking off/on and effort, mode menu, `@` menu, title generation, live shell output and Stop (process ends), compaction divider, history reopen with diffs, add selection and selection comments, editor reload after agent edits, rewind by editing a message (files revert), Open as Markdown, `@` thread summary, question forms, subagent card and preview, skill chips, Follow, Reload Agent, thread restore after restart.

Still open:
- Effort labels: decided to keep OpenCode's names.
- A new thread mentioning another can get the same generated title (OpenCode's title model copies the mention's name). Not Zed's code.
- A directory read that is still running has a location until its output arrives, which logs one "reading bytes" error.

Open bugs and features are tracked in `OPENCODE_D_TRACKER.md`.

Zed AI features that can't use OpenCode yet. They take models from Zed's own providers (`LanguageModelRegistry`), and OpenCode D isn't one:
- Inline assistant in the editor and terminal: "Configure at least one LLM provider".
- Git commit message generation.
A fix would expose OpenCode's models as a Zed language model provider. OpenCode has no plain completion route today (`generate` runs inside a session), so this needs a design.

## 15a. Spike results

Code: `crates/opencode_agent` (new crate; no existing Zed crate changed). Run the live check with:

```
cargo run -p opencode_agent --example follow_session -- --session <id> --seconds 300 --drop-after 200
```

**What it contains**

- `service.rs`: reads `service.json` (`$XDG_STATE_HOME` or `~/.local/state`, the same rule as OpenCode), finds the system `opencode` (`PATH`, then `~/.opencode/bin`). Starting the service isn't wired up yet.
- `client.rs`: HTTP Basic auth; info, sessions, running sessions, messages with paging, create, prompt, interrupt; the event stream with a server-sent-events parser.
- `types.rs`: the session, message and event types we use. Unknown message kinds and event types decode instead of failing. Message IDs are generated in OpenCode's format, checked against a real server ID.
- `session_sync.rs`: `SessionTranscript`, a Rust copy of OpenCode's reducer (`core/src/session/message-updater.ts`) plus live deltas.

**Key design result: every event can be applied twice safely.** Text and reasoning blocks are addressed by their per-message `ordinal`, tools by ID, and derived messages by the event-derived ID. So a client can subscribe, load the snapshot, then apply everything it buffered without duplicates. The reducer asks for a fresh snapshot when:

- a durable sequence number is skipped
- a revert is staged, cleared or committed
- a prompt is delivered that was queued before the client subscribed

**Unit tests (13, all passing; one covers stage, clear and commit of a revert).** The main one replays a full turn with every combination of "subscribed at event i" and "snapshot at event j", and checks each ends exactly as a client that followed the stream throughout.

**Live tests against the shared service (v2.2.5), following this planning session:**

| Run | What it showed | Result |
|---|---|---|
| 20 s, reconnect at 8 s | snapshot of 204 messages, reconnect, resync | PASS |
| 4 min, reconnect at 90 s | streamed text word by word; tool states streaming → running → completed; a subagent's completion delivered mid-run and answered; reconnect | PASS, 210 messages, 0 differences |
| 5 min, reconnect at 200 s | **session idle, then woke itself with no prompt** (subagent finished); prompts typed in the TUI appeared as user messages; reconnect while idle with 0 changes | PASS, 225 messages, 0 differences, 177 events |

**Found along the way**

- The message list rejects `cursor` combined with `order`; the cursor already encodes the order.
- Durable sequence numbers were contiguous per session in every live run, so gap detection works as a safety net.
- With the `claude-agent` provider, each reasoning block arrives as one delta followed by `ended`. Text streams in many small deltas.

| 4 min, throwaway session, reverts made in the TUI | undo revert → all messages visible; revert again → 3 of 7 visible; new prompt → revert committed, resync, 4 messages removed, new turn streamed | PASS, 6 messages, revert point matches |

**Revert, as learned from the live test.** A staged revert deletes nothing. It sets `revert: {messageID}` on the session, and clients hide that message and everything after it (as OpenCode's own client does, `client/src/solid/data.ts:1214-1224`). Undoing it clears the field. Only a commit, which happens on the next prompt, deletes messages. `SessionTranscript` tracks the revert point and exposes `visible_messages()`. Stage and clear are applied directly; commit triggers a resync. For Zed: a staged revert from any client removes the hidden entries from the thread, and clearing it re-appends them.

## 16. Key references

**Zed** (`crates/`):

- `acp_thread/src/connection.rs:95-316`: `AgentConnection`
- `acp_thread/src/acp_thread.rs`: thread API (section 4.3)
- `agent_servers/src/custom.rs:198`, `agent_servers/src/agent_servers.rs`: registration hook
- `agent_ui/src/thread_import.rs`, `agent_ui/src/thread_metadata_store.rs`: import and sidebar store
- `agent_ui/src/config_options.rs`: generic dropdowns and toggles (incl. Claude's Fast)
- `project/src/agent_server_store.rs:1118-1300`: registry binary download
- `settings_ui/src/pages/external_agents_page.rs:156-224`: agent rows in settings
- `sidebar/src/sidebar.rs:6489-6571`: Open as Markdown gate

**OpenCode** (`~/devcave/opencode/packages/`):

- `protocol/openapi.json`, `protocol/src/groups/*.ts`: API
- `schema/src/*.ts`: data shapes
- `client/src/solid/connection.ts`, `client/src/solid/data.ts`: reference client
- `client/src/effect/service.ts`, `client/src/service-probe.ts`, `cli/src/services/server-connection.ts`: service discovery and version handling
- `cli/src/acp/`: the current ACP bridge
- `core/src/config/plugin/{instruction,skill,mcp,compatibility}.ts`: rules, skills, MCP
- `core/src/session/revert.ts`, `core/src/session/subagent-completion.ts`
