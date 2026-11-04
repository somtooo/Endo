#!/usr/bin/env python3
"""Run Zed and Cua in a private, no-bus Hyprland session."""

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time


def no_bus_environment(runtime, home):
    """Build a fresh environment without inherited desktop or bus addresses."""
    runtime = str(runtime)
    home = str(home)
    for name, value in (("runtime", runtime), ("home", home)):
        path = PurePosixPath(value)
        if not path.is_absolute() or ".." in path.parts or any(c in value for c in ",;=\n\0"):
            raise ValueError(f"{name} must be an absolute, unambiguous path")
    if len(runtime.encode()) > 18:
        raise ValueError("runtime path is too long for Cua's Hyprland sockets; use --temp-parent /tmp")
    if runtime == "/" or home == "/" or home == runtime or not home.startswith(runtime.rstrip("/") + "/"):
        raise ValueError("home must be a separate directory inside the private runtime")
    no_bus = f"unix:path={runtime}/no-bus"
    return {
        "PATH": "/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "HOME": home,
        "XDG_RUNTIME_DIR": runtime,
        "XDG_CONFIG_HOME": f"{home}/.config",
        "XDG_DATA_HOME": f"{home}/.local/share",
        "XDG_STATE_HOME": f"{home}/.local/state",
        "XDG_CACHE_HOME": f"{home}/.cache",
        "XDG_SESSION_TYPE": "wayland",
        "XDG_CURRENT_DESKTOP": "Hyprland",
        "DBUS_SESSION_BUS_ADDRESS": no_bus,
        "DBUS_SYSTEM_BUS_ADDRESS": no_bus,
        "AT_SPI_BUS_ADDRESS": no_bus,
        "CUA_DRIVER_RS_ENABLE_WAYLAND": "1",
    }


def check_no_bus_path(runtime):
    """Check that the designated nonexistent bus path does not yet exist."""
    runtime = _validate_private_runtime(runtime)
    try:
        (runtime / "no-bus").lstat()
    except FileNotFoundError:
        return
    raise ValueError("the no-bus path already exists")


def _object(value, allowed, label):
    if not isinstance(value, dict) or value.keys() - allowed:
        raise ValueError(f"unsupported {label} fields")


def _boolean(value, label):
    if type(value) is not bool:
        raise ValueError(f"{label} must be boolean")


def _text(value, label):
    if not isinstance(value, str) or not value or len(value) > 4096:
        raise ValueError(f"{label} must be nonempty text of at most 4096 characters")


def _number(value, label):
    if type(value) not in (int, float) or not (-100000 <= value <= 100000):
        raise ValueError(f"{label} must be a finite bounded number")


def _validate_private_runtime(runtime):
    runtime = Path(runtime)
    if not runtime.is_absolute() or runtime.resolve(strict=True) != runtime:
        raise ValueError("runtime must be an absolute path without symlinks")
    directory = runtime.lstat()
    if (not stat.S_ISDIR(directory.st_mode) or directory.st_uid != os.getuid()
            or stat.S_IMODE(directory.st_mode) != 0o700):
        raise ValueError("private runtime must be owned by this user and mode 0700")
    return runtime


def validate_private_socket(runtime, socket_path, name="driver.sock"):
    """Check a proposed driver socket without connecting to it."""
    runtime = _validate_private_runtime(runtime)
    socket_path = Path(socket_path)
    if name not in ("driver.sock", "control.sock") or socket_path != runtime / name:
        raise ValueError("socket must be the named endpoint in the private runtime")
    endpoint = socket_path.lstat()
    if (not stat.S_ISSOCK(endpoint.st_mode) or endpoint.st_uid != os.getuid()
            or stat.S_IMODE(endpoint.st_mode) & 0o077):
        raise ValueError("driver socket must be private and owned by this user")


def window_request(tool, arguments, pid, window_id):
    """Prepare a narrowly scoped request; this function does not contact Cua."""
    if type(pid) is not int or pid <= 0 or type(window_id) is not int or window_id <= 0:
        raise ValueError("a verified positive Zed pid and window id are required")
    if tool == "get_window_state":
        _object(arguments, {"query", "include_screenshot"}, "window observation")
        if "query" in arguments:
            _text(arguments["query"], "query")
        if "include_screenshot" in arguments:
            _boolean(arguments["include_screenshot"], "include_screenshot")
        return {"pid": pid, "window_id": window_id, "session": "zed-cua-test",
                "include_screenshot": False, **arguments}
    if tool != "run_steps":
        raise ValueError("only window observations and restricted actions are allowed")
    _object(arguments, {"steps", "observe", "delivery_mode"}, "batch")
    delivery_mode = arguments.get("delivery_mode", "background")
    if delivery_mode not in ("background", "foreground"):
        raise ValueError("delivery_mode must be background or foreground")
    steps = arguments.get("steps")
    if not isinstance(steps, list) or not 1 <= len(steps) <= 16:
        raise ValueError("steps must contain 1 to 16 actions")
    action_fields = {
        "click": {"x", "y"},
        "double_click": {"x", "y"},
        "scroll": {"x", "y", "direction", "amount"},
        "drag": {"from_x", "from_y", "to_x", "to_y"},
        "press_key": {"key"},
        "hotkey": {"keys"},
        "type_text": {"text"},
    }
    clean_steps = []
    for step in steps:
        _object(step, {"tool", "args"}, "step")
        name = step.get("tool")
        if not isinstance(name, str) or name not in action_fields:
            raise ValueError("action is not allowed")
        fields = step.get("args")
        _object(fields, action_fields[name], "action")
        if set(fields) != action_fields[name]:
            raise ValueError("action fields must exactly match its allowed schema")
        if name in ("click", "double_click", "scroll"):
            _number(fields["x"], "x")
            _number(fields["y"], "y")
            if name == "scroll":
                if fields["direction"] not in ("up", "down", "left", "right"):
                    raise ValueError("invalid scroll direction")
                if type(fields["amount"]) is not int or not 1 <= fields["amount"] <= 50:
                    raise ValueError("scroll amount must be an integer from 1 to 50")
        elif name == "drag":
            for field in ("from_x", "from_y", "to_x", "to_y"):
                _number(fields[field], field)
        elif name == "hotkey":
            keys = fields["keys"]
            if not isinstance(keys, list) or not 2 <= len(keys) <= 5:
                raise ValueError("hotkey requires 2 to 5 keys")
            for key in keys:
                _text(key, "key")
        else:
            _text(fields["key" if name == "press_key" else "text"], name)
        clean_steps.append({"tool": name, "args": dict(fields)})
    observe = arguments.get("observe", {})
    _object(observe, {"include_screenshot"}, "observation")
    if "include_screenshot" in observe:
        _boolean(observe["include_screenshot"], "include_screenshot")
    return {"pid": pid, "window_id": window_id, "session": "zed-cua-test", "delivery_mode": delivery_mode,
            "steps": clean_steps, "observe": {"include_screenshot": False, **observe}}


def executable(path):
    candidate = Path(path).expanduser().resolve(strict=True)
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise ValueError(f"not an executable file: {candidate}")
    return str(candidate)


def exited_without_reaping(process):
    """Keep the process-group leader's PID reserved until group cleanup."""
    return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def stop_group(process, timeout=5):
    """Stop a group owned by this supervisor, without using stored or reused PIDs."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return process.wait(timeout=5), False
    deadline = time.monotonic() + timeout
    drained = False
    while time.monotonic() < deadline:
        if exited_without_reaping(process) and not group_has_live_members(process.pid):
            drained = True
            break
        time.sleep(0.05)
    if not drained:
        # The unreaped leader keeps the group ID from being reused here.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    return process.wait(timeout=5), not drained


def group_has_live_members(group_id):
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            text = (entry / "stat").read_text()
            fields = text[text.rfind(")") + 2:].split()
            if int(fields[2]) == group_id and fields[0] != "Z":
                return True
        except (OSError, ValueError, IndexError):
            continue
    return False


def build_cage_command(args, config, status):
    return [args.cage, "-d", "--", "/usr/bin/env", "-u", "LD_PRELOAD", "-u", "LD_LIBRARY_PATH",
            "AQ_BACKEND=wayland", sys.executable, str(Path(__file__).resolve().with_name("hypr-child.py")),
            str(status), args.hyprland, str(config)]


def build_driver_command(driver, runtime, tool, arguments, screenshot=None):
    command = [driver, "call", "--socket", str(runtime / "driver.sock"), tool, json.dumps(arguments)]
    if screenshot is not None:
        command.extend(["--screenshot-out-file", str(screenshot)])
    return command


class Supervisor:
    def __init__(self, args, runtime):
        self.args = args
        self.runtime = runtime
        self.home = runtime / "home"
        self.processes = []
        self.logs = []
        self.environment = no_bus_environment(runtime, self.home)
        self.zed_pid = None
        self.window_id = None

    def spawn(self, name, command, environment):
        log = (self.runtime / f"{name}.log").open("w")
        self.logs.append(log)
        process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.processes.append((name, process))
        return process

    def check_children(self):
        hypr_status = self.runtime / "hypr-exit.json"
        if hypr_status.exists():
            raise RuntimeError(f"Hyprland exited unexpectedly; inspect {hypr_status}")
        for name, process in self.processes:
            if exited_without_reaping(process):
                status = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                raise RuntimeError(f"{name} exited (code={status.si_code}, status={status.si_status}); "
                                   f"see {self.runtime / (name + '.log')}")

    def wait_for(self, label, function):
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            self.check_children()
            value = function()
            if value:
                return value
            time.sleep(0.1)
        raise RuntimeError(f"timed out waiting for {label}")

    def hyprctl(self, *arguments):
        return subprocess.run([self.args.hyprctl, *arguments], env=self.environment, stdin=subprocess.DEVNULL, check=True,
                              capture_output=True, text=True, timeout=5).stdout.strip()

    def driver_call(self, tool, arguments, screenshot=None):
        validate_private_socket(self.runtime, self.runtime / "driver.sock")
        command = build_driver_command(self.args.driver, self.runtime, tool, arguments, screenshot)
        return subprocess.run(command, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=45)

    def instance(self):
        try:
            values = json.loads(self.hyprctl("-j", "instances"))
            if len(values) != 1:
                return None
            item = values[0]
            display = item.get("wl_socket")
            signature = item.get("instance")
            if (not isinstance(display, str) or not isinstance(signature, str)
                    or not display or not signature or "/" in display or "/" in signature
                    or not (self.runtime / display).is_socket()
                    or not any((self.runtime / "hypr" / signature / name).is_socket()
                               for name in (".sock", ".socket.sock"))):
                return None
            return item
        except (OSError, ValueError, TypeError, subprocess.SubprocessError):
            return None

    def output_ready(self):
        try:
            return any(output.get("name", "").startswith("HEADLESS-") and not output.get("disabled")
                       for output in json.loads(self.hyprctl("-j", "monitors")))
        except (OSError, ValueError, TypeError, subprocess.SubprocessError):
            return False

    def zed_windows(self):
        """Windows owned by the Zed process this supervisor started."""
        result = self.driver_call("list_windows", {})
        if result.returncode:
            raise RuntimeError("Cua could not list windows")
        return [window for window in json.loads(result.stdout).get("windows", [])
                if window.get("pid") == self.zed_pid and type(window.get("window_id")) is int]

    def window_ready(self):
        try:
            return next(iter(self.zed_windows()), None)
        except (OSError, ValueError, TypeError, RuntimeError, subprocess.SubprocessError):
            return None

    def target_window(self, window_id):
        """Accept the main window or another window of the same Zed process."""
        if window_id is None or window_id == self.window_id:
            return self.window_id
        if type(window_id) is not int or window_id <= 0:
            raise ValueError("window_id must be a positive integer")
        if not any(window["window_id"] == window_id for window in self.zed_windows()):
            raise ValueError("window_id is not a window of the supervised Zed process")
        return window_id

    def start(self):
        self.home.mkdir(mode=0o700)
        (self.runtime / "artifacts").mkdir(mode=0o700)
        check_no_bus_path(self.runtime)
        config = self.runtime / "hyprland.lua"
        # Zed is a Wayland client here. Xwayland only adds a process that can abort at shutdown.
        config.write_text('hl.monitor({ output = "HEADLESS-1", mode = "1920x1080@60", position = "0x0", scale = 1 })\n'
                          'hl.config({ xwayland = { enabled = false } })\n')
        cage_environment = dict(self.environment)
        cage_environment.update({"WLR_BACKENDS": "headless", "WLR_HEADLESS_OUTPUTS": "1",
                                 "WLR_RENDERER": "gles2", "WLR_LIBINPUT_NO_DEVICES": "1",
                                 "WLR_RENDER_DRM_DEVICE": self.args.render_node})
        if self.args.cage_library_path:
            cage_environment["LD_LIBRARY_PATH"] = self.args.cage_library_path
        if self.args.cage_preload:
            cage_environment["LD_PRELOAD"] = self.args.cage_preload
        self.spawn("compositor", build_cage_command(self.args, config, self.runtime / "hypr-exit.json"), cage_environment)
        instance = self.wait_for("private Hyprland socket", self.instance)
        self.environment["WAYLAND_DISPLAY"] = instance["wl_socket"]
        self.environment["HYPRLAND_INSTANCE_SIGNATURE"] = instance["instance"]
        if self.hyprctl("output", "create", "headless") != "ok":
            raise RuntimeError("Hyprland failed to create the private headless output")
        self.wait_for("private headless output", self.output_ready)
        if self.args.plugin:
            if self.hyprctl("plugin", "load", self.args.plugin) != "ok":
                raise RuntimeError("Cua plugin failed to load")
            with config.open("a") as output:
                output.write('hl.config({ plugin = { cua = { enabled = true } } })\n')
            if self.hyprctl("reload") != "ok":
                raise RuntimeError("Cua plugin configuration failed to reload")
            def input_ready():
                try:
                    status = json.loads(self.hyprctl("-j", "cua:status"))
                    return (status.get("abi", {}).get("match") and status.get("configured")
                            and status.get("input", {}).get("protocol") == 3
                            and status["input"].get("transport_ready")
                            and not status["input"].get("test_only")
                            and all(status.get(key) is True for key in
                                    ("keyboard_layout_independent", "foreground_numlock_compatible", "ime_popup_guard")))
                except (OSError, ValueError, TypeError, subprocess.SubprocessError):
                    return False
            self.wait_for("Cua input transport", input_ready)
        self.spawn("virtual-devices", [self.args.virtual_devices], self.environment)
        def devices_ready():
            return "virtual pointer and keyboard ready" in (self.runtime / "virtual-devices.log").read_text()
        self.wait_for("virtual pointer and keyboard", devices_ready)
        self.zed_pid = self.spawn("zed", [self.args.zed, "--user-data-dir", str(self.runtime / "profile"),
                                          self.args.project], self.environment).pid
        self.spawn("driver", [self.args.driver, "serve", "--socket", str(self.runtime / "driver.sock"),
                              "--pid-file", str(self.runtime / "driver.pid"), "--no-overlay"], self.environment)
        window = self.wait_for("Zed discovery on the private driver", self.window_ready)
        self.window_id = window["window_id"]
        # Float windows opened later (Settings, dialogs) so they don't retile
        # and shrink the main window. Rules apply when a window maps, so the
        # main window stays tiled.
        with config.open("a") as output:
            output.write('hl.window_rule({ name = "float-later-windows", match = { class = ".*" }, float = true })\n')
        if self.hyprctl("reload") != "ok":
            raise RuntimeError("floating window rule failed to reload")

    def handle(self, request):
        _object(request, {"tool", "arguments", "screenshot", "window_id"}, "request")
        tool = request.get("tool")
        if tool == "list_windows":
            fields = ("window_id", "title", "bounds", "z_index")
            windows = [{key: window.get(key) for key in fields} for window in self.zed_windows()]
            return {"ok": True, "main_window_id": self.window_id, "windows": windows, "error": None}
        window_id = self.target_window(request.get("window_id"))
        arguments = window_request(tool, request.get("arguments", {}), self.zed_pid, window_id)
        if tool == "run_steps" and arguments["delivery_mode"] == "foreground" and not self.args.plugin:
            raise ValueError("foreground input requires --plugin")
        screenshot = request.get("screenshot")
        if screenshot is not None:
            if (not isinstance(screenshot, str) or not screenshot.endswith(".png")
                    or Path(screenshot).name != screenshot or screenshot in (".png", "..")):
                raise ValueError("screenshot must be a simple PNG filename")
            enabled = arguments.get("include_screenshot") if tool == "get_window_state" else arguments["observe"].get("include_screenshot")
            if not enabled:
                raise ValueError("screenshot requires include_screenshot=true")
            screenshot = self.runtime / "artifacts" / screenshot
            if screenshot.exists() or screenshot.is_symlink():
                raise ValueError("refusing to overwrite an existing screenshot")
        result = self.driver_call(tool, arguments, screenshot)
        success = result.returncode == 0
        error = None
        if screenshot and success and not screenshot.is_file():
            success = False
            error = "Cua did not write the requested window capture"
        return {"ok": success, "stdout": result.stdout, "stderr": result.stderr, "error": error,
                "screenshot": str(screenshot) if screenshot and success else None}

    def serve(self):
        path = self.runtime / "control.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(path))
            path.chmod(0o600)
            listener.listen(4)
            listener.settimeout(0.2)
            print(json.dumps({"runtime": str(self.runtime), "control": str(path),
                              "zed_pid": self.zed_pid, "window_id": self.window_id}), flush=True)
            while True:
                self.check_children()
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                with connection:
                    connection.settimeout(5)
                    try:
                        data = connection.makefile("rb").readline(65537)
                        if len(data) > 65536 or not data.endswith(b"\n"):
                            raise ValueError("invalid or oversized request")
                        response = self.handle(json.loads(data))
                    except (ValueError, OSError, RuntimeError, subprocess.SubprocessError) as error:
                        response = {"ok": False, "error": str(error)}
                    try:
                        connection.sendall(json.dumps(response).encode() + b"\n")
                    except (BrokenPipeError, ConnectionResetError):
                        continue

    def cleanup(self):
        errors = []
        for name, process in reversed(self.processes):
            try:
                code, forced = stop_group(process)
                if code not in (0, -signal.SIGTERM, -signal.SIGINT):
                    errors.append(f"{name} exited with status {code}")
                if forced:
                    errors.append(f"{name} process group required SIGKILL")
            except (OSError, ChildProcessError, subprocess.TimeoutExpired) as error:
                errors.append(f"{name}: {error}")
        if any(name == "compositor" for name, _ in self.processes):
            try:
                status = json.loads((self.runtime / "hypr-exit.json").read_text())
                code = status["returncode"]
                if type(code) is not int or code not in (0, -signal.SIGTERM, -signal.SIGINT):
                    errors.append(f"Hyprland exited with status {code}")
            except (OSError, ValueError, KeyError, TypeError) as error:
                errors.append(f"Hyprland exit status unavailable: {error}")
        for log in self.logs:
            log.close()
        if errors:
            raise RuntimeError("cleanup failed: " + "; ".join(errors))


def prepare_arguments(args):
    if args.timeout <= 0:
        raise ValueError("timeout must be positive")
    for name in ("cage", "hyprland", "hyprctl", "driver", "virtual_devices", "zed"):
        setattr(args, name, executable(getattr(args, name)))
    if args.plugin:
        args.plugin = str(Path(args.plugin).resolve(strict=True))
        if not Path(args.plugin).is_file():
            raise ValueError("plugin must be a file")
    if args.cage_preload:
        args.cage_preload = str(Path(args.cage_preload).resolve(strict=True))
        if not Path(args.cage_preload).is_file():
            raise ValueError("Cage preload must be a file")
    if args.cage_library_path:
        args.cage_library_path = str(Path(args.cage_library_path).resolve(strict=True))
        if not Path(args.cage_library_path).is_dir():
            raise ValueError("Cage library path must be a directory")
    project = Path(args.project).resolve(strict=True)
    if not project.is_dir():
        raise ValueError("project must be an existing directory")
    args.project = str(project)
    render_node = Path(args.render_node).resolve(strict=True)
    if not stat.S_ISCHR(render_node.stat().st_mode) or not os.access(render_node, os.R_OK | os.W_OK):
        raise ValueError("render node must be a readable and writable character device")
    args.render_node = str(render_node)


def run_supervisor(args):
    if not args.acknowledge_isolated_test:
        raise ValueError("--acknowledge-isolated-test is required")
    prepare_arguments(args)
    old_umask = os.umask(0o077)
    runtime = None
    supervisor = None
    failed = True
    try:
        runtime = Path(tempfile.mkdtemp(prefix="z-", dir=args.temp_parent)).resolve(strict=True)
        supervisor = Supervisor(args, runtime)
        supervisor.start()
        supervisor.serve()
    except KeyboardInterrupt:
        failed = False
    finally:
        try:
            if supervisor:
                supervisor.cleanup()
        except Exception:
            failed = True
            raise
        finally:
            os.umask(old_umask)
            if runtime:
                if failed or args.keep:
                    print(f"retained run data: {runtime}", file=sys.stderr)
                else:
                    shutil.rmtree(runtime)


def control_call(args):
    runtime = Path(args.runtime).resolve(strict=True)
    path = runtime / "control.sock"
    validate_private_socket(runtime, path, name="control.sock")
    arguments = json.loads(args.arguments)
    request = {"tool": args.tool, "arguments": arguments}
    if args.window_id is not None:
        request["window_id"] = args.window_id
    if args.screenshot:
        request["screenshot"] = args.screenshot
    payload = json.dumps(request).encode() + b"\n"
    if len(payload) > 65536:
        raise ValueError("request is too large")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(60)
        connection.connect(str(path))
        connection.sendall(payload)
        response = json.loads(connection.makefile("rb").readline())
    print(json.dumps(response, indent=2))
    return 0 if response.get("ok") else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="start a private session")
    run.add_argument("--acknowledge-isolated-test", action="store_true")
    run.add_argument("--zed", required=True)
    run.add_argument("--project", required=True)
    run.add_argument("--cage", required=True)
    run.add_argument("--hyprland", required=True)
    run.add_argument("--hyprctl", required=True)
    run.add_argument("--driver", required=True)
    run.add_argument("--virtual-devices", required=True)
    run.add_argument("--render-node", required=True)
    run.add_argument("--plugin")
    run.add_argument("--cage-preload")
    run.add_argument("--cage-library-path")
    run.add_argument("--temp-parent", default="/tmp")
    run.add_argument("--timeout", type=float, default=30)
    run.add_argument("--keep", action="store_true")
    call = commands.add_parser("call", help="send a window-only request")
    call.add_argument("--runtime", required=True)
    call.add_argument("tool", choices=("get_window_state", "run_steps", "list_windows"))
    call.add_argument("arguments", nargs="?", default="{}", help="JSON object")
    call.add_argument("--window-id", type=int,
                      help="another window of the supervised Zed process (see list_windows)")
    call.add_argument("--screenshot", help="new PNG filename under the run's artifacts directory")
    args = parser.parse_args()
    if args.command == "run":
        if not args.acknowledge_isolated_test:
            parser.error("--acknowledge-isolated-test is required")
        signal.signal(signal.SIGTERM, lambda signum, frame: (_ for _ in ()).throw(KeyboardInterrupt()))
        run_supervisor(args)
        return 0
    return control_call(args)


if __name__ == "__main__":
    sys.exit(main())
