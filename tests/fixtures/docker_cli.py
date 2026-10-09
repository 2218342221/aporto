#!/usr/bin/python3
"""Stateful Docker lifecycle fixture; the fake socket selects isolated test state."""

import json
import pathlib
import sys
import time

assert sys.argv[1] == "--host" and sys.argv[2].startswith("unix://")
root = pathlib.Path(sys.argv[2][len("unix://") :]).parent
args = sys.argv[3:]
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args[:2]) + "\n")

id = "a" * 64
image = "sha256:" + "b" * 64
state_path = root / "state.json"
state = (
    json.loads(state_path.read_text())
    if state_path.exists()
    else {"exists": False, "running": False}
)


def save():
    state_path.write_text(json.dumps(state))


if args[:2] == ["image", "inspect"]:
    print(json.dumps({"Id": image, "Config": {"Volumes": None}}))
elif args[:2] == ["container", "create"]:
    state.update(exists=True, running=False)
    save()
    print(id)
elif args[:2] == ["container", "inspect"]:
    if not state["exists"]:
        print("No such container", file=sys.stderr)
        sys.exit(1)
    print(
        json.dumps(
            {
                "Id": id,
                "Image": image,
                "Config": {
                    "Labels": {
                        "io.aporto.managed": "true",
                        "io.aporto.owner": "fixture-owner",
                        "io.aporto.image": image,
                    }
                },
                "State": {"Running": state["running"], "Paused": False},
            }
        )
    )
elif args[:2] == ["container", "start"]:
    state["running"] = True
    save()
elif args[:2] in (["container", "stop"], ["container", "rm"]):
    action = args[1]
    (root / (action + ".entered")).write_text("1")
    while (root / ("hold-" + action)).exists():
        time.sleep(0.005)
    if (root / ("fail-" + action)).exists():
        sys.exit(1)
    state = json.loads(state_path.read_text())
    state["running"] = False
    if action == "rm":
        state["exists"] = False
    save()
elif args[:2] == ["container", "exec"]:
    if "-i" in args:
        request = json.loads(sys.stdin.readline())
        if "argv" in request:
            print(json.dumps({"kind": "started", "pid": 42}), flush=True)
            if request["stdin"]:
                sys.stdin.read()
            print(
                json.dumps(
                    {
                        "kind": "exit",
                        "code": 1 if (root / "fail-contract").exists() else 0,
                    }
                ),
                flush=True,
            )
    else:
        (root / "ready.entered").write_text("1")
        while (root / "hold-ready").exists():
            time.sleep(0.005)
        if (root / "fail-ready").exists():
            sys.exit(1)
else:
    raise RuntimeError("unsupported fake CLI operation")
