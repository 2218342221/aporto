"""Cancel one supervised guest process group, including starts still in flight."""
import json
import os
import signal
import sys

tag = json.loads(sys.stdin.buffer.readline(1024))["tag"]
if not tag or any(c not in "0123456789abcdef-" for c in tag):
    raise ValueError("invalid process tag")
root = "/tmp/.aporto-processes"
os.makedirs(root, mode=0o700, exist_ok=True)
with open(os.path.join(root, tag + ".cancel"), "a"):
    pass
try:
    with open(os.path.join(root, tag + ".json")) as stream:
        record = json.load(stream)
    pid = record["pid"]
    if not isinstance(pid, int) or pid <= 1:
        raise ValueError("invalid process PID")
    with open("/proc/%d/stat" % pid) as stream:
        current = stream.read().rsplit(")", 1)[1].split()[19]
    if current == record["start"]:
        os.killpg(pid, signal.SIGKILL)
except (FileNotFoundError, ProcessLookupError):
    pass
