"""Bounded startup probe, independent of the not-yet-initialized process registry."""
import os
from pathlib import Path
import sys
import time

start = Path("/proc/1/stat").read_text().rsplit(")", 1)[1].split()[19]
identity = start + ":" + os.readlink("/proc/1/ns/pid")
deadline = time.monotonic() + 8
while True:
    try:
        if Path("/tmp/.aporto-ready").read_text() == identity:
            break
    except (OSError, UnicodeError):
        pass
    if time.monotonic() >= deadline:
        sys.exit("container initialization timed out")
    time.sleep(0.02)
