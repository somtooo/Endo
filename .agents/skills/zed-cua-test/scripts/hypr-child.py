#!/usr/bin/env python3
"""Record the nested Hyprland child's exit status for the supervisor."""

import json
from pathlib import Path
import signal
import subprocess
import sys


def main(arguments):
    if len(arguments) != 3:
        raise ValueError("expected status path, Hyprland executable, and configuration")
    status, hyprland, config = arguments
    signal.signal(signal.SIGTERM, lambda signum, frame: None)
    signal.signal(signal.SIGINT, lambda signum, frame: None)
    child = subprocess.Popen([hyprland, "--config", config])
    code = child.wait()
    path = Path(status)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps({"pid": child.pid, "returncode": code}) + "\n")
    temporary.replace(path)
    return 0 if code in (0, -signal.SIGTERM, -signal.SIGINT) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
