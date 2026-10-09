"""Aporto container boot. Publish readiness only after initialization."""
import os
from pathlib import Path
import shutil
import signal
import sys
import uuid

signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
os.makedirs("/workspace", exist_ok=True)
os.makedirs("/opt/agent", exist_ok=True)
shutil.rmtree("/tmp/.aporto-processes", ignore_errors=True)

# Docker --init is PID 1. Its kernel start time and namespace distinguish a new
# start from the previous container's marker, which survives in its filesystem.
start = Path("/proc/1/stat").read_text().rsplit(")", 1)[1].split()[19]
identity = start + ":" + os.readlink("/proc/1/ns/pid")
temporary = Path("/tmp/.aporto-ready-" + uuid.uuid4().hex)
temporary.write_text(identity)
os.replace(temporary, "/tmp/.aporto-ready")
signal.pause()
