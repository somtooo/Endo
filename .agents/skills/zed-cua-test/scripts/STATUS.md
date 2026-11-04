# How the Zed test harness works

`zedtest` is the entry point. It prepares the host, then starts `headless.py`,
which runs one private Hyprland, Cua driver and Zed, and serves a control
socket that `zedtest`'s action commands talk to.

## What `up` prepares

Everything is cached in `~/.cache/zed-cua-test/v1` (override with
`ZEDTEST_CACHE`) under a lock, so parallel agents prepare it once:

- **Cage:** a kiosk compositor that hosts the nested Hyprland. Cage 0.3.1
  needs wlroots 0.20, which Omarchy doesn't install, so `zedtest` downloads
  the exact Arch packages (`cage`, `wlroots0.20`, `libliftoff`) from the Arch
  archive, checks their pinned SHA-256 sums and unpacks them into the cache.
  Nothing is installed system-wide. `cage-xdg-v6.c` is a small preload that
  lets this Cage accept clients asking for xdg-shell version 6.
- **Cua plugin:** built from `../cua-hyprland-plugin` (see its README) for the
  installed Hyprland. It is rebuilt when the source, Hyprland version or
  compiler changes. It is loaded only into the private Hyprland, never into
  the user's desktop.
- **Virtual devices:** `virtual-devices.c` creates a virtual pointer and
  keyboard on the private Hyprland.
- **Zed:** `cargo build --locked -p zed --bin zed --features wayland-first-seat`
  in the worktree. Debug builds read assets from their checkout, so the
  binary runs in place; a later rebuild doesn't affect a running Zed.

Each run's files are in `~/.cache/zed-cua-test/v1/runs/NAME` (metadata, logs,
private OpenCode home); its sample project is in `/tmp/zt-project-NAME-*`. The supervisor's runtime is a short
`/tmp/z-*` directory (Cua's sockets need a short path) that holds Zed's
profile, logs and the `artifacts` folder with screenshots. `down` keeps both;
`down --purge` deletes them.

## What the supervisor does

The run owns a new mode-0700 runtime, a separate HOME and XDG directories,
and a nonexistent D-Bus address. It inherits neither the host display,
compositor signature, D-Bus addresses nor linker overrides. Cage uses its
headless backend, and nested Hyprland gets a minimal configuration with one
headless output. Zed and a separate Cua daemon start only after the output
and virtual devices are ready.

The supervisor discovers the Zed window through its own driver socket and
binds every call to that Zed process. Calls go to the first window unless
they name another window of the same process. The control socket accepts only
`list_windows`, `get_window_state` and restricted window-bound `run_steps`;
it rejects desktop capture, recording, arbitrary tools, scripts and windows of
other processes. Windows opened after the first one float, so they don't
retile and shrink the main window. Screenshots use only the window API and
never overwrite a file.

Each launched process has its own process group, stopped in reverse order at
shutdown. A forced SIGKILL, abnormal exit or missing Hyprland exit record
counts as a cleanup failure, and `down` reports it. `down` also stops any
process still carrying the run's runtime or private OpenCode HOME, and lists
new crash records from `coredumpctl` for Hyprland, Cage, Zed, Cua and OpenCode
(with parallel runs, a crash may belong to another run; the report says so).

This is accidental isolation for trusted local agents, binaries and projects,
not a security boundary. A same-user process could reach the driver socket.
The project stays writable and Zed can use the network.

## Known limits

- **Crash risk in capture:** Cua's Hyprland window capture goes through
  Hyprland's screen-sharing code, which has crashed at shutdown in earlier
  experiments (`coredumpctl info 1311698`). Report any crash; don't suppress
  crash reporting.
- **Background input** is refused for Zed, so every action uses foreground
  delivery inside the private compositor.
- **Punctuation keys** have no key mapping in the driver; `type` them, or use
  the command palette.
- **Hover** has no driver tool. `hover` sends one sideways wheel click, which
  moves the pointer first. It could nudge a horizontally scrollable view.
- **No D-Bus:** portals, file pickers, notifications and the accessibility
  tree are unavailable, so actions use screenshot pixels.
- **Fresh profile:** every run starts with default settings, closed panels and
  no credentials. `--settings FILE` merges extra settings. To test data from
  an earlier profile, use `headless.py run` directly with a `--zed` wrapper
  that copies a saved `db` directory into the `--user-data-dir` it receives.
- **Input seat:** the `wayland-first-seat` feature keeps GPUI on the first
  Wayland seat, because Cua adds extra seats and GPUI manages only one. A
  build without it can lose keyboard and pointer input. This is not full
  multi-seat support.

## OpenCode

`--opencode private` starts `opencode serve --service` with HOME and XDG
directories under the run, after `opencode service set port` picks a free
port, so it never collides with the user's service on the default port.
Zed's wrapper exports only `XDG_STATE_HOME`, which is where Zed finds
`opencode/service.json`. The service starts with no sessions.

Logins are piped from the user's service (`opencode auth export`) into the
private one (`opencode auth import`), leaving out GitHub Copilot. Claude Agent
stores no key: `CLAUDE_CONFIG_DIR` points the service at `~/.claude`, like a
second Claude Code window. `down` deletes the private database that holds the
copies, then stops the service with `opencode service stop`, or by signal if
needed.

**Risk with the OpenAI login:** every copy holds the same refresh token, and
OpenAI may replace it at renewal. If a test service renews it, the user's real
service can be logged out and need a new `opencode auth login`; a copy still
running after the real service renews can stop working. Runs that finish
before the token's expiry don't renew it. `--no-logins` avoids this entirely.

Sample projects live in `/tmp` because OpenCode walks up from the project
looking for skills: under HOME it finds the user's `~/.claude` and `~/.agents`
(45 skills instead of the 3 built-in ones).

`--opencode real` instead links the user's own `service.json` into a private
state directory. Everything the test does then reaches the real service:
sessions, file edits and model usage. Use it only when the user asks.

## Troubleshooting

- `up` prints the root cause when startup fails;
  `/tmp/zt-NAME log --source supervisor` shows the rest. A Zed panic is shown from the run's `zed.log`.
- "timed out waiting for Cua input transport": the plugin was built without
  `CUA_HYPRLAND_INPUT=ON`, or it doesn't match the running Hyprland.
- Plugin build failures after a Hyprland update: update the vendored plugin as
  its README describes.
- To see raw input events, add `export WAYLAND_DEBUG=client` to the run's
  `zed-wrapper` before starting and look for `wl_pointer` lines in `zed.log`.
  That's how the missing scroll detents were found.
- Unit tests: `python3 -B -m unittest discover -s .agents/skills/zed-cua-test/scripts -p 'test_*.py'`.
