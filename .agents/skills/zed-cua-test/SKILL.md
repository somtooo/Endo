---
name: zed-cua-test
description: Drive a private, isolated Zed on Linux Wayland (Omarchy/Hyprland) with clicks, typing, keys and screenshots. Use it to test Zed UI changes ("test zed", check how a worktree's build behaves), and more generally to take the human out of the loop - whenever you need to see or click something in Zed to validate a change, spawn an agent with this skill instead of asking the user. Works for many agents in parallel. Each agent starts its own Zed with one command, drives it with short commands, and stops it with one command.
---

# Test Zed with zedtest

This skill is for more than testing. It takes the human out of the loop: any
time you'd otherwise ask the user to click something in Zed, look at a screen,
or confirm a UI behaves a certain way, run it yourself here, or hand a
subagent one specific thing to validate ("open the profile picker and check
that Configure opens OpenCode's settings").

**Hard rule: drive Zed only through this skill's `zedtest` and `/tmp/zt-NAME`
commands.** Never call the `cua-driver` program or any desktop-control tool
directly, even if `zt` seems limited: those reach the user's real desktop, not
your test run. If `zt` can't do something, report that as a blocker.

The script is `scripts/zedtest` inside this skill's directory (below:
`zedtest`); call it by its absolute path. Run `up` from inside the worktree
under test, or pass `--worktree DIR`; using the script from a different
worktree's copy of this skill is fine. The worktree must contain the "Fix
Wayland input" commit from `develop`.

## The run

1. **Start:** `zedtest up`. It builds the worktree's Zed if needed, prepares
   everything else, and starts a private Zed with a fresh profile and a
   sample project (a git repo with `README.md`, `notes.txt`, `src/main.py`).
   It prints a command such as `/tmp/zt-a1b2` and the path of a first
   screenshot. A cold build takes many minutes: give `up` a 40-minute timeout
   and wait for it. If the build fails, report the error; never test a stale
   binary.
2. **Drive:** `/tmp/zt-a1b2 COMMAND`. Every action prints `ok` or
   `action failed`, then saves a screenshot and prints its path. Look at a
   screenshot only when you need to see the result.
3. **Stop:** `/tmp/zt-a1b2 down`, always, even after a failure. It prints
   `clean` or a list of problems (crashes, leftover processes); report any
   problems.

`up` options: `--project DIR` (must be disposable; keep it outside HOME, or
OpenCode loads the skills in the user's `~/.claude` and `~/.agents`),
`--opencode private` (default is none, and then OpenCode D can't connect; see
below), `--settings FILE` (JSON merged into Zed's settings), `--name`,
`--worktree DIR`, `--ask-trust` (keep the project trust prompt),
`--no-build` (only right after `zedtest build`: a plain `cargo run` builds
without the input feature the harness needs, and then every key fails).

| Command | Does |
| --- | --- |
| `shot [NAME]` | screenshot; NAME (or `--label NAME`) is added to the file name, and files never overwrite |
| `click X Y [--double]` | click |
| `type "TEXT" [--enter]` | type text; about 25 ms per character, so 2,000 characters take about a minute |
| `key KEY...` | press keys in order: `return`, `escape`, `tab`, `up`, `down`, `end`, `backspace`, `ctrl+s`, `ctrl+shift+p` |
| `palette "ACTION" [--search]` | run a command palette action, e.g. `palette "pane: split right"`. It can't tell whether the name matched. `--search` types it and leaves the palette open, so the screenshot shows the matches. The palette lists only actions available where focus is, so "No matches" can mean focus is in the wrong place |
| `scroll X Y up/down/left/right [CLICKS]` | wheel scroll, 3 clicks by default and up to 50 (about 60 px each; 10 clicks pages through a window without skipping anything, 30 reaches the end of most pages) |
| `hover X Y` | move the pointer there, to reveal hover-only icons and tooltips (it sends one sideways wheel click) |
| `drag X1 Y1 X2 Y2` | drag |
| `windows` | this Zed's windows and IDs |
| `log [-e] [--all] [-n N] [--grep RE] [--source zed/stdout/supervisor/opencode/build]` | end of Zed's log; `-e` keeps only errors and warnings, merges repeats and hides lines every fresh profile logs (`--all` shows them) |
| `api PATH [-X METHOD] [-d JSON] [--full]` | call the private OpenCode service |
| `evidence "TITLE" SHOT... --level L [--steps --expected --actual --where --test --notes]` | add a finding with its screenshots to the run's `REPORT.md` (see Reporting) |
| `status`, `down [--purge]` | check or stop the run |

All of these go through the run's `/tmp/zt-NAME` command. Only `up`,
`build`, `setup` and `list` are run as `zedtest` itself.

Action options: `-w ID` targets another window of this Zed, `--no-shot`
skips the screenshot, `--wait S` waits longer before it (default 0.6), and
`--label WORD` names it.

## Guidelines

- **Decide the expected result before acting.** `ok` only means the input
  was sent. Evidence is a screenshot you looked at, a file's contents, a log
  line or an `api` response.
- **Coordinates** are pixels in the latest screenshot of that window, as is.
  The screenshot is smaller than the window; don't scale anything. After
  anything that changes the view (Back, a new page, a scroll, a dropdown),
  take a fresh screenshot before clicking: old coordinates hit the wrong
  control.
- **Focus:** clicking the project panel leaves focus there, so typing would
  go into the panel (and can start a rename). Click inside the editor or
  input before typing. After opening anything, look before typing.
- **The UI is asynchronous.** If a screenshot shows the old state, take
  another `shot` (or use `--wait 2`) before repeating an action. Panes,
panels and windows opened from the palette often need `--wait 2`. Don't
  repeat actions blindly.
- **Keys:** punctuation can't be pressed as a key (`/`, `?`, `,` fail). Type
  punctuation with `type`, and use `palette` for shortcuts that need it, for
  example `palette "agent: toggle focus"` instead of `ctrl+?`.
- **Extra windows** such as Settings float over the main one, but the main
  window's screenshot doesn't show them. Actions print `other windows open`
  with their IDs; pass `-w ID` to `shot`, `click` and the rest.
- **Save tokens:** chain sure steps with `--no-shot`, then take one `shot`.
- **Logs:** agent and model errors often appear only in the thread itself,
  not in Zed's log.
- **Opening files:** click them in the project panel, or use
  `palette "file finder: toggle"` and type the name. Don't use "workspace:
  open files" or other system dialogs: the run has no D-Bus, so system
  dialogs and portals fail with "ZBus Error". Expect that error for any
  feature that needs a portal, and note it rather than retrying.
- **Agent panel:** open it with `palette "agent: toggle focus"`. Its `+`
  menu starts new threads: Zed Agent, Terminal, and external agents such as
  OpenCode D; "Add More Agents" opens the ACP registry tab. The clock icon
  (`ctrl+g`) shows thread history. The `...` menu has MCP servers, profiles,
  settings and the threads sidebar.
- If the same step fails twice in the same way, stop and report it instead
  of looping.

## Reporting findings

Record every finding with `evidence` as soon as you've confirmed it, then
mention it in your final report. Each one goes into the run's `REPORT.md`
(in its `/tmp/z-*` folder, next to the screenshots) with the images inline,
so a person can open it in Zed's Markdown preview and check it. Every action
is also logged in `commands.log` there, so the run can be replayed.

- `--level unit-test`: a failing unit test shows the bug. Use it for logic
  that runs without the UI (settings files, parsing, state, service
  replies). Write tests only when your task allows code changes and you have
  your own worktree; never commit them. Put the test name, the `cargo test`
  command and the failure in `--test`.
- `--level ui-confirmed`: you reproduced it in the UI. Give the screenshots
  before and after, the steps, expected and actual, and `--where` (which
  part of which image shows it).
- `--level unconfirmed`: seen once, not reproduced. Say so.

A finding needs at least one screenshot that clearly shows it. Describe what
you saw, not guesses about the cause. Example:
`zt evidence "History row doesn't open its thread" 010 011 --level ui-confirmed --steps "Open history (ctrl+g), click the only row" --expected "The thread opens" --actual "The panel stays on the previous thread" --where "011: the panel header still says OpenCode D"`.
When several testers ran, `zedtest reports` writes `/tmp/zedtest-reports.md`
with links to every report.

## OpenCode

For OpenCode D or anything that needs OpenCode, start with
`zedtest up --opencode private`. That run gets its own OpenCode service on
its own port with an empty history, plus a copy of the user's logins (OpenAI
and Claude Agent), so messages get real replies. Replies are part of the
test: send messages, don't mark them blocked. Keep prompts short, because
they use the user's quota.

### Choosing a model

Use only these models, set in the thread's model selector and effort
selector. Each new thread starts on a free model, so change it before the
first message. Don't use free models, the "Fast" versions, or any other
model.

| Model | Provider | Effort | When to use it |
|---|---|---|---|
| GPT-6 Luna | OpenAI | max | The default. It's the cheapest. |
| Haiku 5.5 | Claude Agent | xhigh | When a test needs faster replies. It costs a little more. |
| GPT-6.1 Terra | OpenAI | high | When a test needs a stronger model, or as a fast fallback. It costs the most of the three. |

A 429 or "usage limit" in the thread means that account is out of quota.
Switch to an allowed model from the other provider. If none of the three
work, report it as a blocker, not a Zed bug. `--no-logins` leaves only the
free models, so use it only when a test needs no replies.

Inspect the service with `api /api/session` (sessions) and
`api /api/session/ID/message` (a session's messages); add `--full` to pipe
long output into a parser. Service errors are in `log --source opencode`.

`down` deletes the copied logins from the run. Never use `--opencode real`
unless the user asks for it explicitly: that connects to the user's real
service, where deleting a thread deletes a real session.

## Running many testers (for the controller)

- Don't wait for one tester before starting the next. Runs are isolated, so as
  soon as you have new work to verify, build and start a tester for it, even
  while others run. A rebuild doesn't disturb a running Zed. Runs started after
  it use the new binary, so tell testers which build they cover.
- Repeat the hard rule above in every tester's prompt. A tester used desktop
  tools by mistake once, so spell it out.
- Each tester runs its own `up` … `down`; runs never share a Zed, even in
  the same worktree. Give each one the skill, the worktree path, what to test
  and how to report: steps, expected and actual results, screenshot paths and
  relevant log lines.
- Build each worktree once before spawning testers
  (`zedtest build --worktree DIR`, 40-minute timeout), so their `up` takes
  seconds. Testers on one worktree share its build.
- Each Zed takes about 1 to 2 GB of memory; keep about four to six at once.
- `zedtest list` shows all runs. Stop a forgotten one with
  `/tmp/zt-NAME down`.

## Safety

Each run has its own compositor, profile, HOME and no D-Bus, so it never
touches the user's desktop, windows or settings. Don't kill processes by name;
`down` stops only what the run started. This skill never tests on the user's
visible desktop.
How the harness works, its limits and troubleshooting are also in
`scripts/STATUS.md`.
