import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stderr
from io import StringIO
from types import SimpleNamespace
from unittest import mock

import headless


class EnvironmentTests(unittest.TestCase):
    def test_fresh_environment_ignores_live_desktop(self):
        with mock.patch.dict(os.environ, {
            "DISPLAY": ":0", "WAYLAND_DISPLAY": "wayland-live",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1000/bus",
            "DBUS_STARTER_ADDRESS": "unix:path=/run/user/1000/bus",
            "HYPRLAND_INSTANCE_SIGNATURE": "live", "LD_PRELOAD": "/bad.so",
        }, clear=True):
            environment = headless.no_bus_environment("/tmp/z-test", "/tmp/z-test/home")
        self.assertEqual(environment["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/tmp/z-test/no-bus")
        self.assertEqual(environment["DBUS_SYSTEM_BUS_ADDRESS"], environment["DBUS_SESSION_BUS_ADDRESS"])
        for name in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_STARTER_ADDRESS", "HYPRLAND_INSTANCE_SIGNATURE", "LD_PRELOAD"):
            self.assertNotIn(name, environment)

    def test_invalid_paths_fail(self):
        for runtime, home in (("relative", "/tmp/home"), ("/", "/home"),
                              ("/tmp/z", "/home/user"), ("/tmp/z", "/tmp/z"),
                              ("/tmp/z,evil", "/tmp/z,evil/home"),
                              ("/tmp/" + "a" * 30, "/tmp/" + "a" * 30 + "/home"),
                              ("/tmp/z/../x", "/tmp/z/home")):
            with self.subTest(runtime=runtime, home=home), self.assertRaises(ValueError):
                headless.no_bus_environment(runtime, home)

    def test_runtime_must_leave_space_for_plugin_socket(self):
        runtime = "/tmp/" + "a" * 14
        with self.assertRaisesRegex(ValueError, "too long"):
            headless.no_bus_environment(runtime, runtime + "/home")

    def test_no_bus_path_rejects_files_and_dangling_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "no-bus"
            headless.check_no_bus_path(directory)
            path.touch()
            with self.assertRaises(ValueError):
                headless.check_no_bus_path(directory)
            path.unlink()
            path.symlink_to("missing")
            with self.assertRaises(ValueError):
                headless.check_no_bus_path(directory)

    def test_launcher_requires_opt_in(self):
        args = ["headless.py", "run", "--zed", "/example/zed", "--project", "/example/project",
                "--cage", "/cage", "--hyprland", "/hypr", "--hyprctl", "/hyprctl", "--driver", "/driver",
                "--virtual-devices", "/devices", "--render-node", "/dev/node"]
        with mock.patch.object(sys, "argv", args), redirect_stderr(StringIO()) as error:
            with self.assertRaises(SystemExit) as exit_status:
                headless.main()
        self.assertEqual(exit_status.exception.code, 2)
        self.assertIn("--acknowledge-isolated-test is required", error.getvalue())


class WindowPolicyTests(unittest.TestCase):
    def test_window_observation_is_bound_to_window(self):
        result = headless.window_request("get_window_state", {"include_screenshot": True}, 100, 200)
        self.assertEqual(result, {"pid": 100, "window_id": 200, "session": "zed-cua-test", "include_screenshot": True})
        self.assertFalse(headless.window_request("get_window_state", {}, 100, 200)["include_screenshot"])

    def test_foreground_action_is_bound_to_window(self):
        result = headless.window_request("run_steps", {
            "delivery_mode": "foreground",
            "steps": [{"tool": "hotkey", "args": {"keys": ["ctrl", "shift", "p"]}}],
            "observe": {"include_screenshot": True},
        }, 100, 200)
        self.assertEqual((result["pid"], result["window_id"], result["delivery_mode"]), (100, 200, "foreground"))
        self.assertEqual(result["session"], "zed-cua-test")
        self.assertEqual(result["observe"], {"include_screenshot": True})

    def test_background_is_default(self):
        result = headless.window_request("run_steps", {
            "steps": [{"tool": "press_key", "args": {"key": "enter"}}],
        }, 100, 200)
        self.assertEqual(result["delivery_mode"], "background")

    def test_window_pointer_actions(self):
        steps = [
            {"tool": "double_click", "args": {"x": 1, "y": 2}},
            {"tool": "scroll", "args": {"x": 1, "y": 2, "direction": "down", "amount": 3}},
            {"tool": "drag", "args": {"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}},
        ]
        self.assertEqual(headless.window_request("run_steps", {"steps": steps}, 100, 200)["steps"], steps)

    def test_reject_desktop_and_arbitrary_tools(self):
        for name in ("get_desktop_state", "run_script", "click", "list_windows", "unknown"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                headless.window_request(name, {}, 100, 200)

    def test_reject_target_and_scope_overrides(self):
        cases = [
            ("get_window_state", {"pid": 1}),
            ("get_window_state", {"target": {"kind": "desktop"}}),
            ("get_window_state", {"capture_mode": "desktop"}),
            ("run_steps", {"pid": 1, "steps": []}),
            ("run_steps", {"delivery_mode": "desktop", "steps": []}),
            ("run_steps", {"steps": [{"tool": "click", "args": {"x": 1, "y": 2, "scope": "desktop"}}]}),
            ("run_steps", {"steps": [{"tool": "click", "args": {"x": 1, "y": 2}, "expect": {}}]}),
            ("run_steps", {"steps": [{"tool": "run_script", "args": {}}]}),
            ("run_steps", {"steps": [{"tool": [], "args": {}}]}),
            ("run_steps", {"steps": [{"tool": "click", "args": {"x": True, "y": 2}}]}),
            ("run_steps", {"steps": [{"tool": "click", "args": {"x": float("nan"), "y": 2}}]}),
            ("run_steps", {"steps": [{"tool": "scroll", "args": {"x": 1, "y": 2, "direction": "down", "amount": 51}}]}),
            ("run_steps", {"steps": [{"tool": "drag", "args": {"from_x": 1, "from_y": 2, "to_x": 3, "to_y": float("inf")}}]}),
            ("run_steps", {"steps": [{"tool": "click", "args": {"x": 1, "y": 2}}], "observe": {"target": {"kind": "desktop"}}}),
        ]
        for name, args in cases:
            with self.subTest(name=name, args=args), self.assertRaises(ValueError):
                headless.window_request(name, args, 100, 200)

    def test_invalid_identity_fails(self):
        for pid, window in ((0, 1), (True, 1), (1, -1), (1, "2")):
            with self.subTest(pid=pid, window=window), self.assertRaises(ValueError):
                headless.window_request("get_window_state", {}, pid, window)


class OwnershipTests(unittest.TestCase):
    def test_only_private_owned_socket_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            path = runtime / "driver.sock"
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(path))
                path.chmod(0o600)
                headless.validate_private_socket(runtime, path)
                with self.assertRaises(ValueError):
                    headless.validate_private_socket(runtime, runtime / "other.sock")
                path.chmod(0o666)
                with self.assertRaises(ValueError):
                    headless.validate_private_socket(runtime, path)
                path.chmod(0o600)
                runtime.chmod(0o755)
                with self.assertRaises(ValueError):
                    headless.validate_private_socket(runtime, path)


class SupervisorTests(unittest.TestCase):
    def test_start_uses_no_bus_and_starts_devices_before_zed(self):
        with tempfile.TemporaryDirectory() as directory:
            args = SimpleNamespace(cage="/cage", hyprland="/hypr", render_node="/dev/node",
                                   cage_library_path=None, cage_preload=None, plugin=None,
                                   virtual_devices="/devices", zed="/zed", project="/project",
                                   driver="/driver", timeout=1)
            supervisor = headless.Supervisor(args, Path(directory))
            observed = []
            def spawn(name, command, environment):
                observed.append((name, command, dict(environment)))
                return SimpleNamespace(pid=444)
            def wait_for(label, predicate):
                if label == "private Hyprland socket":
                    return {"wl_socket": "wayland-private", "instance": "private-instance"}
                if label == "Zed discovery on the private driver":
                    return {"window_id": 555}
                return True
            with mock.patch.object(supervisor, "spawn", side_effect=spawn), \
                 mock.patch.object(supervisor, "wait_for", side_effect=wait_for), \
                 mock.patch.object(supervisor, "hyprctl", return_value="ok"):
                supervisor.start()
            self.assertEqual([item[0] for item in observed], ["compositor", "virtual-devices", "zed", "driver"])
            self.assertEqual(observed[2][1], ["/zed", "--user-data-dir", str(Path(directory) / "profile"), "/project"])
            for name, _, environment in observed:
                self.assertEqual(environment["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={directory}/no-bus")
                self.assertNotIn("DISPLAY", environment)
                if name != "compositor":
                    self.assertEqual(environment["WAYLAND_DISPLAY"], "wayland-private")
                    self.assertEqual(environment["HYPRLAND_INSTANCE_SIGNATURE"], "private-instance")
            self.assertEqual((supervisor.zed_pid, supervisor.window_id), (444, 555))

    def test_commands_target_private_socket_and_drop_cage_linker_overrides(self):
        args = SimpleNamespace(cage="/cage", hyprland="/hypr")
        command = headless.build_cage_command(args, Path("/tmp/z/config"), Path("/tmp/z/status"))
        self.assertEqual(command[:8], ["/cage", "-d", "--", "/usr/bin/env", "-u", "LD_PRELOAD", "-u", "LD_LIBRARY_PATH"])
        driver = headless.build_driver_command("/driver", Path("/tmp/z"), "get_window_state", {"pid": 1})
        self.assertEqual(driver[:5], ["/driver", "call", "--socket", "/tmp/z/driver.sock", "get_window_state"])

    def test_cleanup_only_signals_owned_groups_and_reports_crash(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            (runtime / "hypr-exit.json").write_text('{"pid": 1, "returncode": -11}')
            supervisor = headless.Supervisor(SimpleNamespace(), runtime)
            processes = [mock.Mock(pid=101), mock.Mock(pid=202)]
            supervisor.processes = [("compositor", processes[0]), ("driver", processes[1])]
            with mock.patch.object(headless, "stop_group", side_effect=[(0, False), (0, False)]) as stop:
                with self.assertRaisesRegex(RuntimeError, "Hyprland exited with status -11"):
                    supervisor.cleanup()
            self.assertEqual(stop.call_args_list, [mock.call(processes[1]), mock.call(processes[0])])

    def test_missing_compositor_status_is_not_success(self):
        with tempfile.TemporaryDirectory() as directory:
            supervisor = headless.Supervisor(SimpleNamespace(), Path(directory))
            supervisor.processes = [("compositor", mock.Mock(pid=101))]
            with mock.patch.object(headless, "stop_group", return_value=(0, False)):
                with self.assertRaisesRegex(RuntimeError, "exit status unavailable"):
                    supervisor.cleanup()

    def test_stop_group_keeps_leader_until_group_signals(self):
        process = mock.Mock(pid=123)
        process.wait.return_value = 0
        events = []
        with mock.patch.object(headless.os, "killpg", side_effect=lambda pid, sig: events.append(("signal", pid, sig))), \
             mock.patch.object(headless, "exited_without_reaping", return_value=True), \
             mock.patch.object(headless, "group_has_live_members", return_value=False):
            self.assertEqual(headless.stop_group(process), (0, False))
        self.assertEqual(events, [("signal", 123, headless.signal.SIGTERM)])
        process.wait.assert_called_once_with(timeout=5)

    def test_cleanup_kills_owned_group_after_leader_exits(self):
        code = ("import subprocess,sys\n"
                "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], "
                "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n"
                "print(child.pid,flush=True)\n")
        process = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE,
                                   text=True, start_new_session=True)
        try:
            self.assertTrue(process.stdout.readline().strip().isdigit())
            for _ in range(100):
                if headless.exited_without_reaping(process):
                    break
                time.sleep(0.01)
            else:
                self.fail("test group leader did not exit")
            code, forced = headless.stop_group(process, timeout=2)
            self.assertEqual(code, 0)
            self.assertFalse(forced)
            self.assertFalse(headless.group_has_live_members(process.pid))
        finally:
            if process.returncode is None:
                headless.stop_group(process, timeout=1)
            process.stdout.close()

    def test_forced_cleanup_is_reported(self):
        process = mock.Mock(pid=123)
        process.wait.return_value = -headless.signal.SIGKILL
        with mock.patch.object(headless.os, "killpg") as kill, \
             mock.patch.object(headless, "exited_without_reaping", return_value=False):
            self.assertEqual(headless.stop_group(process, timeout=0), (-headless.signal.SIGKILL, True))
        self.assertEqual(kill.call_args_list, [mock.call(123, headless.signal.SIGTERM), mock.call(123, headless.signal.SIGKILL)])

    def test_rejects_desktop_and_requires_plugin_for_actions(self):
        with tempfile.TemporaryDirectory() as directory:
            supervisor = headless.Supervisor(SimpleNamespace(plugin=None), Path(directory))
            supervisor.zed_pid, supervisor.window_id = 1, 2
            with self.assertRaises(ValueError):
                supervisor.handle({"tool": "get_desktop_state", "arguments": {}})
            with self.assertRaisesRegex(ValueError, "requires --plugin"):
                supervisor.handle({"tool": "run_steps", "arguments": {"delivery_mode": "foreground", "steps": [{"tool": "click", "args": {"x": 1, "y": 2}}]}})

    def test_capture_is_bound_to_window_and_missing_file_is_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            (runtime / "artifacts").mkdir()
            supervisor = headless.Supervisor(SimpleNamespace(plugin=None), runtime)
            supervisor.zed_pid, supervisor.window_id = 123, 456
            result = SimpleNamespace(returncode=0, stdout="{}", stderr="")
            with mock.patch.object(supervisor, "driver_call", return_value=result) as call:
                response = supervisor.handle({"tool": "get_window_state", "arguments": {"include_screenshot": True}, "screenshot": "before.png"})
            self.assertFalse(response["ok"])
            self.assertIn("did not write", response["error"])
            call.assert_called_once_with("get_window_state", {"pid": 123, "window_id": 456, "session": "zed-cua-test", "include_screenshot": True}, runtime / "artifacts" / "before.png")
            with self.assertRaises(ValueError):
                supervisor.handle({"tool": "get_window_state", "arguments": {"include_screenshot": True}, "screenshot": "../bad.png"})

    def test_targets_only_windows_of_the_supervised_zed(self):
        with tempfile.TemporaryDirectory() as directory:
            supervisor = headless.Supervisor(SimpleNamespace(plugin=None), Path(directory))
            supervisor.zed_pid, supervisor.window_id = 123, 456
            windows = json.dumps({"windows": [
                {"pid": 123, "window_id": 456, "title": "project"},
                {"pid": 123, "window_id": 789, "title": "Zed — Settings"},
                {"pid": 999, "window_id": 111, "title": "other app"},
            ]})
            listing = SimpleNamespace(returncode=0, stdout=windows, stderr="")
            state = SimpleNamespace(returncode=0, stdout="{}", stderr="")

            def driver_call(tool, arguments, screenshot=None):
                return listing if tool == "list_windows" else state

            with mock.patch.object(supervisor, "driver_call", side_effect=driver_call) as call:
                listed = supervisor.handle({"tool": "list_windows"})
                self.assertEqual([window["window_id"] for window in listed["windows"]], [456, 789])
                self.assertEqual(listed["main_window_id"], 456)
                supervisor.handle({"tool": "get_window_state", "arguments": {}, "window_id": 789})
                self.assertEqual(call.call_args.args[1]["window_id"], 789)
                for window_id in (111, 0, "789"):
                    with self.assertRaises(ValueError):
                        supervisor.handle({"tool": "get_window_state", "arguments": {}, "window_id": window_id})


if __name__ == "__main__":
    unittest.main()
