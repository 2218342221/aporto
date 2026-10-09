"""Trusted docker-exec supervisor; application argv/env arrive only over stdin."""
import base64
import json
import os
import selectors
import signal
import subprocess
import sys
import threading
import time

REGISTRY = "/tmp/.aporto-processes"


def emit(kind, **values):
    print(json.dumps(dict(kind=kind, **values), separators=(",", ":")), flush=True)


def start_time(pid):
    with open("/proc/%d/stat" % pid) as stream:
        return stream.read().rsplit(")", 1)[1].split()[19]


process = None
reaped = False
record = None
cancel = None
try:
    request = json.loads(sys.stdin.buffer.readline(1024 * 1024 + 1))
    tag = request["tag"]
    if not tag or any(c not in "0123456789abcdef-" for c in tag):
        raise ValueError("invalid process tag")
    os.makedirs(REGISTRY, mode=0o700, exist_ok=True)
    record = os.path.join(REGISTRY, tag + ".json")
    cancel = os.path.join(REGISTRY, tag + ".cancel")
    if os.path.exists(cancel):
        raise RuntimeError("process start cancelled")
    environment = dict(os.environ)
    environment.update(request["env"])
    process = subprocess.Popen(
        request["argv"],
        cwd=request["cwd"],
        env=environment,
        stdin=subprocess.PIPE if request["stdin"] else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    # A unique cancellation tombstone closes the start/kill race before PID exists.
    with open(record + ".tmp", "x") as stream:
        json.dump({"pid": process.pid, "start": start_time(process.pid)}, stream)
    os.replace(record + ".tmp", record)
    if os.path.exists(cancel):
        os.killpg(process.pid, signal.SIGKILL)
        raise RuntimeError("process start cancelled")
    emit("started", pid=process.pid)

    if request["stdin"]:
        def forward_input():
            try:
                while True:
                    data = os.read(0, 32768)
                    if not data:
                        break
                    process.stdin.write(data)
                    process.stdin.flush()
            except (OSError, ValueError):
                pass
            finally:
                try:
                    process.stdin.close()
                except OSError:
                    pass

        threading.Thread(target=forward_input, daemon=True).start()

    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    maximum = request["max_output"]
    total = 0
    deadline = time.monotonic() + request["timeout_ms"] / 1000 if request["timeout_ms"] else None
    running = True
    while selector.get_map() or running:
        if deadline is not None and time.monotonic() >= deadline:
            raise TimeoutError("process timed out")
        if os.path.exists(cancel):
            raise RuntimeError("process cancelled")
        # WNOWAIT reserves the PID until its group has been killed.
        running = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None
        if not running:
            # A shell that leaves background children must not leave their pipes open.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for key, _ in selector.select(0.05):
            data = os.read(key.fileobj.fileno(), 32768)
            if not data:
                selector.unregister(key.fileobj)
                continue
            total += len(data)
            if total > maximum:
                raise RuntimeError("process output exceeds limit")
            emit(key.data, data=base64.b64encode(data).decode("ascii"))
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    code = process.wait()
    reaped = True
    emit("exit", code=code if code >= 0 else 128 - code)
except BaseException as error:
    # Exception details may contain caller argv/env/path values. Keep them private.
    message = "process timed out" if isinstance(error, TimeoutError) else "guest process failed or was cancelled"
    emit("error", message=message)
finally:
    if process is not None and not reaped:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
    for path in (record, cancel):
        if path is not None:
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
