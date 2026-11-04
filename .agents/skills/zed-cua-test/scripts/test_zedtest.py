import importlib.machinery
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

loader = importlib.machinery.SourceFileLoader("zedtest", str(Path(__file__).with_name("zedtest")))
spec = importlib.util.spec_from_loader("zedtest", loader)
zedtest = importlib.util.module_from_spec(spec)
loader.exec_module(zedtest)


class ZedtestTests(unittest.TestCase):
    def test_key_steps(self):
        self.assertEqual(zedtest.key_step("return"), {"tool": "press_key", "args": {"key": "return"}})
        self.assertEqual(zedtest.key_step("ctrl+shift+p"), {"tool": "hotkey", "args": {"keys": ["ctrl", "shift", "p"]}})
        with self.assertRaises(zedtest.Failure):
            zedtest.key_step("+")

    def test_settings_merge_keeps_unrelated_keys(self):
        merged = zedtest.merged(zedtest.DEFAULT_SETTINGS, {"session": {"restore_unsaved_buffers": False}})
        self.assertEqual(merged["session"], {"trust_all_worktrees": True, "restore_unsaved_buffers": False})
        self.assertEqual(zedtest.DEFAULT_SETTINGS["session"], {"trust_all_worktrees": True})

    def test_run_names(self):
        for name in ("a1b2", "oc-tester-1"):
            self.assertTrue(zedtest.NAME_PATTERN.match(name))
        for name in ("", "-x", "A", "a/b", "x" * 21):
            self.assertFalse(zedtest.NAME_PATTERN.match(name))

    def test_shot_names_count_up_and_drop_unsafe_characters(self):
        with tempfile.TemporaryDirectory() as directory:
            meta = {"run_dir": directory, "shots": 0}
            (Path(directory) / "meta.json").write_text(json.dumps(meta))
            self.assertEqual(zedtest.next_shot_name(meta, None), "001.png")
            self.assertEqual(zedtest.next_shot_name(meta, "../settings page"), "002-settings-page.png")
            self.assertEqual(json.loads((Path(directory) / "meta.json").read_text())["shots"], 2)

    def test_long_text_is_typed_in_pieces(self):
        text = "x" * (zedtest.TYPE_CHUNK_CHARACTERS * 2 + 1)
        chunks = zedtest.text_chunks(text)
        self.assertEqual(len(chunks), 3)
        self.assertEqual("".join(chunks), text)
        self.assertEqual(zedtest.text_chunks("short"), ["short"])

    def test_log_repeats_collapse_and_harmless_lines_match(self):
        lines = ["2026-11-04T15:52:23-05:00 ERROR a", "2026-11-04T15:52:24-05:00 ERROR b",
                 "2026-11-04T15:52:25-05:00 ERROR a", "timestamp=2026-11-04T23:07:09Z level=ERROR c"]
        self.assertEqual(zedtest.collapse_repeats(lines),
                         ["2026-11-04T15:52:24-05:00 ERROR b", "2026-11-04T15:52:25-05:00 ERROR a  (x2)",
                          "timestamp=2026-11-04T23:07:09Z level=ERROR c"])
        self.assertTrue(zedtest.HARMLESS_LOG_LINES.search('canonicalizing "crates/grammars/src": No such file'))
        self.assertFalse(zedtest.HARMLESS_LOG_LINES.search("Failed to generate summary"))

    def test_png_size(self):
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / "a.png"
            image.write_bytes(b"\x89PNG\r\n\x1a\n" + b"\0\0\0\rIHDR" + (1441).to_bytes(4, "big") + (797).to_bytes(4, "big"))
            self.assertEqual(zedtest.png_size(image), (1441, 797))


if __name__ == "__main__":
    unittest.main()
