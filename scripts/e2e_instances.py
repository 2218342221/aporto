#!/usr/bin/env python3
"""Exercise runtime selection and shared instances through real Core/HTTP processes.

Docker uses two already-installed images and actual guest commands/MCP/files.
AgentENV local/remote modes use a bounded HTTP/protobuf fixture; no microVM or
host shell is used for simulated guest commands. Responses is always a fixture.
"""
import argparse
import email.parser
import email.policy
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request

from e2e_stack import Fixture, HTTP, frame, toml_value


def varint(value):
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 127) | 128)
        value >>= 7
    return bytes(encoded + bytes([value]))


def field(number, payload):
    return varint(number << 3 | 2) + varint(len(payload)) + payload


def fields(payload):
    """Decode only protobuf wire types used by the pinned ProcessConfig fixture."""
    result, cursor = {}, 0

    def integer():
        nonlocal cursor
        value, shift = 0, 0
        while True:
            assert cursor < len(payload) and shift <= 63
            byte = payload[cursor]
            cursor += 1
            value |= (byte & 127) << shift
            if byte < 128:
                return value
            shift += 7

    while cursor < len(payload):
        tag = integer()
        if tag & 7 == 0:
            value = integer()
        else:
            assert tag & 7 == 2
            length = integer()
            value = payload[cursor:cursor + length]
            assert len(value) == length
            cursor += length
        result.setdefault(tag >> 3, []).append(value)
    return result


class InstanceFixture(Fixture):
    def sandbox(self):
        identifier = self.headers["x-agentenv-sandbox-id"]
        assert self.headers["X-Access-Token"] == "envd-" + identifier
        return self.server.sandboxes[identifier]

    def do_GET(self):
        path = urlsplit(self.path)
        if path.path != "/files":
            return self.reply(status=404)
        target = parse_qs(path.query)["path"][0]
        with self.server.lock:
            value = self.sandbox()["files"].get(target)
        self.reply(value if value is not None else b"missing", 200 if value is not None else 404,
                   "application/octet-stream")

    def do_DELETE(self):
        identifier = urlsplit(self.path).path.removeprefix("/sandboxes/")
        assert self.headers["X-API-Key"] == "instance-runtime-key"
        with self.server.lock:
            assert identifier in self.server.sandboxes
            del self.server.sandboxes[identifier]
            self.server.deleted.append(identifier)
        self.reply(status=204)

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        path = urlsplit(self.path)
        state = self.server
        if path.path == "/v1/responses":
            return self.respond_model(json.loads(data))
        if path.path == "/v2/sandboxes":
            assert self.headers["X-API-Key"] == "instance-runtime-key"
            template = json.loads(data)["templateID"]
            with state.lock:
                identifier = "instance-" + str(len(state.created) + 1)
                state.created.append(template)
                state.sandboxes[identifier] = {"template": template, "files": {}}
            return self.reply({"sandboxID": identifier, "envdAccessToken": "envd-" + identifier})
        if path.path.startswith("/v2/sandboxes/") and path.path.endswith("/connect"):
            assert self.headers["X-API-Key"] == "instance-runtime-key"
            identifier = path.path.split("/")[3]
            with state.lock:
                assert identifier in state.sandboxes
                state.connected.append(identifier)
            return self.reply({"sandboxID": identifier, "envdAccessToken": "envd-" + identifier})
        if path.path.startswith("/sandboxes/"):
            assert self.headers["X-API-Key"] == "instance-runtime-key"
            with state.lock:
                identifier = path.path.split("/")[2]
                assert identifier in state.sandboxes
                if path.path.endswith("/pause"):
                    state.paused.append(identifier)
            return self.reply(status=204)
        if path.path == "/files":
            target = parse_qs(path.query)["path"][0]
            message = email.parser.BytesParser(policy=email.policy.default).parsebytes(
                b"Content-Type: " + self.headers["Content-Type"].encode() + b"\r\n\r\n" + data)
            content = next(message.iter_parts()).get_payload(decode=True)
            with state.lock:
                self.sandbox()["files"][target] = content
            return self.reply({})
        if path.path == "/process.Process/Start":
            assert len(data) >= 5 and data[0] == 0
            request = fields(data[5:])
            process = fields(request[1][0])
            assert process[1][0] == b"/bin/sh"
            command = process[2][-1].decode()
            cwd = process[4][0].decode()
            with state.lock:
                self.sandbox()
                state.command_workdirs.append(cwd)
            # Model only the fixed probe; setup commands have no filesystem side
            # effects beyond the isolated in-memory file dictionary.
            if command == "pwd; python3 --version":
                stdout = (cwd + "\nPython fixture\n").encode()
            else:
                assert command.startswith("mkdir -p -- "), "unexpected fixture command"
                stdout = b""
            events = frame(field(1, field(1, b"\x08\x2a")))
            if stdout:
                events += frame(field(1, field(2, field(1, stdout))))
            events += frame(field(1, field(3, b"")))
            return self.reply(events, content_type="application/connect+proto")
        if path.path == "/process.Process/SendSignal":
            return self.reply(b"", content_type="application/proto")
        self.reply(status=404)

    def respond_model(self, request):
        assert request["stream"] is True and request["parallel_tool_calls"] is False
        assert {tool["name"] for tool in request["tools"]} == {"exec", "wait"}
        inputs = request["input"]
        last_user = max(i for i, item in enumerate(inputs) if item.get("role") == "user")
        task = json.loads(inputs[last_user]["content"])
        label = task["label"]
        outputs = [item for item in inputs[last_user + 1:] if item.get("type") == "custom_tool_call_output"]
        if not outputs:
            users = [json.loads(item["content"])["label"] for item in inputs if item.get("role") == "user"]
            assert users == task["prior"] + [label], "reuse leaked another thread's model history"
            assert any(item.get("role") == "developer" and
                       "Session working directory: " + json.dumps(task["workdir"]) in item.get("content", "")
                       for item in inputs), "selected workdir absent from model context"
            with self.server.lock:
                self.server.started_labels.add(label)
            if task.get("hold"):
                self.server.hold_started.set()
                assert self.server.hold_release.wait(30), "shared-instance scheduling probe timed out"
            source = '// @exec: {"yield_time_ms":60000,"max_output_tokens":5000}\n'
            source += "text(await tools.exec_command({command:'pwd; python3 --version'}));\n"
            if self.server.docker:
                source += "text(await tools.mcp__probe__runtime_version({}));\n"
            if task["action"] == "write":
                source += "text(await tools.write_file({path:'persistent.txt',content:" + json.dumps(task["receipt"]) + "}));\n"
            source += "text(await tools.read_file({path:'persistent.txt'}));"
            output = [{"type": "message", "id": label + "-note", "role": "assistant", "status": "completed",
                       "phase": "commentary", "content": [{"type": "output_text", "text": "检查运行环境。"}]},
                      {"type": "custom_tool_call", "name": "exec", "call_id": "instance-" + label, "input": source}]
        else:
            assert len(outputs) == 1
            result = json.loads(outputs[0]["output"])
            assert result["status"] == "completed", "PTC probe did not complete"
            values = [json.loads(value) if isinstance(value, str) else value for value in result["output"]]
            assert values[0]["exit_code"] == 0
            assert values[0]["stdout"].startswith(task["workdir"] + "\nPython " + task["python"])
            assert values[-1]["path"] == task["workdir"] + "/persistent.txt"
            assert values[-1]["content"] == task["receipt"]
            if self.server.docker:
                assert values[1]["isError"] is False
                assert values[1]["content"][0]["text"] == task["python"]
            with self.server.lock:
                self.server.completed_labels.add(label)
            output = [{"type": "message", "id": label + "-final", "role": "assistant", "status": "completed",
                       "phase": "final_answer", "content": [{"type": "output_text", "text": task["receipt"]}]}]
        # Reuse the tested SSE encoder; number 0 disables that script's own gate.
        self.responses_stream(output, 0)


MCP_PROBE = '''import json, sys
version = ".".join(map(str, sys.version_info[:2]))
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request: continue
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"image-probe","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"runtime_version","description":"Report Python " + version,"inputSchema":{"type":"object","properties":{},"additionalProperties":False}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":version}],"isError":False}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--runtime", choices=["docker", "agentenv"], default="docker")
    parser.add_argument("--mode", choices=["local", "remote"], default="local")
    parser.add_argument("--docker-host", default="unix:///var/run/docker.sock")
    parser.add_argument("--default-image", default="python:3.12-slim")
    parser.add_argument("--alternate-image", default="python:3.11-slim")
    args = parser.parse_args()
    if args.runtime == "docker" and args.mode != "local":
        parser.error("Docker is local only")
    binary = args.bin_dir.resolve()
    fixture = ThreadingHTTPServer(("127.0.0.1", 0), InstanceFixture)
    fixture.lock = threading.Lock()
    fixture.docker = args.runtime == "docker"
    fixture.sandboxes, fixture.created, fixture.deleted, fixture.connected, fixture.paused = {}, [], [], [], []
    fixture.command_workdirs = []
    fixture.started_labels, fixture.completed_labels = set(), set()
    fixture.hold_started, fixture.hold_release = threading.Event(), threading.Event()
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{fixture.server_port}"
    token = secrets.token_hex(32)
    images = [args.default_image, args.alternate_image] if fixture.docker else ["template-default", "template-alternate"]
    versions = ["3.12", "3.11"] if fixture.docker else ["fixture", "fixture"]
    started = time.monotonic()
    report, process, log = None, None, None
    release_id = None
    with tempfile.TemporaryDirectory(prefix="aporto-instances-") as temporary:
        directory = Path(temporary)
        environment = dict(os.environ, INSTANCE_SERVER_TOKEN=token, INSTANCE_MODEL_KEY="instance-model-key",
                           INSTANCE_RUNTIME_KEY="instance-runtime-key")

        def docker(*arguments):
            return subprocess.check_output(["docker", "--host", args.docker_host, *arguments], stderr=subprocess.PIPE, timeout=30)

        try:
            runtime = {"provider": args.runtime, "image" if fixture.docker else "template": images[0],
                       "images" if fixture.docker else "templates": [images[1]], "workdir": "/projects/default"}
            agentfile = '[agent]\nname="instance-test"\n[model]\nname="fixture"\nconnection="primary"\n[runtime]\n'
            agentfile += "".join(key + "=" + toml_value(value) + "\n" for key, value in runtime.items())
            if fixture.docker:
                (directory / "mcp").mkdir()
                (directory / "mcp/server.py").write_text(MCP_PROBE)
                agentfile += '\n[[mcp]]\nname="probe"\nsource="mcp"\ncommand=["python3","server.py"]\n'
            (directory / "Agentfile").write_text(agentfile)
            bundle = directory / "instance.agent.json"
            subprocess.run([str(binary / "aporto"), "build", str(directory), "-o", str(bundle)], check=True, capture_output=True)
            binding = {"host": args.docker_host} if fixture.docker else {
                "mode": args.mode, "api_url": endpoint, "sandbox_url": endpoint,
                "api_key": {"env": "INSTANCE_RUNTIME_KEY"}, "lease_ms": 30000}
            config = '[connections.primary]\nendpoint=' + toml_value(endpoint + "/v1/responses") + '\napi_key={env="INSTANCE_MODEL_KEY"}\n'
            config += '\n[[agents]]\nid="instance-test"\nbundle=' + toml_value(str(bundle)) + '\n[agents.runtime]\n'
            config += "".join(key + "=" + toml_value(value) + "\n" for key, value in binding.items())
            deployment = directory / "deployment.toml"
            deployment.write_text(config)
            subprocess.run([str(binary / "aporto"), "activate", "--config", str(deployment), "--agent", "instance-test"],
                           env=environment, check=True, capture_output=True, timeout=120)
            release_id = json.loads((directory / "releases/active.json").read_text())["instance-test"]
            release = json.loads((directory / "releases" / (release_id[7:] + ".release.json")).read_text())
            assert set(release["variants"]) == {images[1]}
            if fixture.docker:
                default_catalog = release["catalog"]["tools"]["mcp__probe__runtime_version"]
                alternate_catalog = release["variants"][images[1]]["catalog"]["tools"]["mcp__probe__runtime_version"]
                assert default_catalog["definition"]["description"] == "Report Python " + versions[0]
                assert alternate_catalog["definition"]["description"] == "Report Python " + versions[1]
            else:
                assert fixture.created == images and len(fixture.deleted) == 2
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            base = f"http://127.0.0.1:{port}"
            log = (directory / "server.log").open("w+")

            def api(path, data=None, expected=200):
                headers = {"Authorization": "Bearer " + token}
                if data is not None:
                    headers["Content-Type"] = "application/json"
                request = Request(base + path, None if data is None else json.dumps(data).encode(), headers)
                try:
                    response = HTTP.open(request, timeout=15)
                except HTTPError as error:
                    response = error
                accepted = (expected,) if isinstance(expected, int) else expected
                assert response.status in accepted, f"{path}: unexpected HTTP {response.status}"
                return json.load(response)

            def launch():
                child = subprocess.Popen([str(binary / "aporto-server"), "--core-bin", str(binary / "aporto-core"),
                    "--core-config", str(deployment), "--state-dir", str(directory / "state"),
                    "--listen", f"127.0.0.1:{port}", "--token-env", "INSTANCE_SERVER_TOKEN"],
                    env=environment, stdout=log, stderr=log)
                for _ in range(100):
                    assert child.poll() is None, "HTTP server exited at startup"
                    try:
                        api("/v1/agents")
                        return child
                    except (URLError, ConnectionError):
                        time.sleep(0.05)
                child.kill()
                child.wait()
                raise RuntimeError("HTTP server startup deadline exceeded")

            def task(label, action, receipt, workdir, version, prior=None, hold=False):
                return {"label": label, "action": action, "receipt": receipt, "workdir": workdir,
                        "python": version, "prior": prior or [], "hold": hold}

            def submit(thread, specification):
                return api(f"/v1/threads/{thread['id']}/turns", {
                    "input": json.dumps(specification), "idempotency_key": specification["label"]}, expected=202)

            def wait_turn(thread, turn, specification):
                for _ in range(400):
                    result = api("/v1/threads/" + thread["id"])
                    state = next(value for value in result["turns"] if value["id"] == turn["id"])
                    if state["status"] in ("completed", "failed", "interrupted"):
                        assert state["status"] == "completed", f"turn ended {state['status']}: {state.get('error')}"
                        assert state["output"] == specification["receipt"]
                        items = api(f"/v1/threads/{thread['id']}/turns/{turn['id']}/items")["items"]
                        tools = [item for item in items if item["kind"] == "tool_call"]
                        assert any(item["name"] == "read_file" and item["status"] == "completed" for item in tools)
                        if specification["action"] == "read":
                            assert all(item["name"] != "write_file" for item in tools)
                        return result["thread"]
                    time.sleep(0.1)
                raise RuntimeError("turn completion deadline exceeded")

            def verify_file(thread, specification):
                identifier = thread["sandbox_id"]
                if fixture.docker:
                    info = json.loads(docker("inspect", identifier))[0]
                    assert info["Config"]["Labels"]["io.aporto.owner"] == "aporto-" + release_id[7:]
                    assert info["State"]["Status"] == "exited"
                    assert not info["HostConfig"]["Privileged"] and not info["Mounts"]
                    identity = release["runtime_identity"] if thread["runtime_image"] == images[0] else release["variants"][images[1]]["runtime_identity"]
                    assert info["Image"] == identity["image_id"]
                    target = directory / (thread["id"] + ".txt")
                    docker("cp", identifier + ":" + specification["workdir"] + "/persistent.txt", str(target))
                    assert target.read_text() == specification["receipt"]
                else:
                    assert fixture.sandboxes[identifier]["template"] == thread["runtime_image"]
                    assert fixture.sandboxes[identifier]["files"][specification["workdir"] + "/persistent.txt"].decode() == specification["receipt"]

            process = launch()
            options = api("/v1/agents")["agents"][0]["runtime"]
            assert options == {"provider": args.runtime, "images": images, "default_image": images[0], "default_workdir": "/projects/default"}
            assert api("/v1/agents/instance-test/instances")["instances"] == []
            for invalid in [
                {"runtime": {"mode": "new", "image": "unlisted"}},
                {"workdir": "/opt/agent/private"},
                {"runtime": {"mode": "reuse", "instance_id": "not-an-aporto-instance"}},
            ]:
                api("/v1/threads", {"agent_id": "instance-test", **invalid}, expected=(400, 404))
            first = api("/v1/threads", {"agent_id": "instance-test", "title": "default"}, expected=201)
            first_task = task("first", "write", "first-" + secrets.token_hex(12), "/projects/default", versions[0])
            first = wait_turn(first, submit(first, first_task), first_task)
            verify_file(first, first_task)
            second = api("/v1/threads", {"agent_id": "instance-test", "title": "alternate",
                "runtime": {"mode": "new", "image": images[1]}, "workdir": "/projects/custom project"}, expected=201)
            second_task = task("second", "write", "second-" + secrets.token_hex(12), "/projects/custom project", versions[1])
            second = wait_turn(second, submit(second, second_task), second_task)
            verify_file(second, second_task)
            assert first["sandbox_id"] != second["sandbox_id"]
            assert first["runtime_instance_id"] != second["runtime_instance_id"]
            instances = api("/v1/agents/instance-test/instances")["instances"]
            assert {item["id"] for item in instances} == {first["runtime_instance_id"], second["runtime_instance_id"]}
            assert all(not item["busy"] for item in instances)
            reused = api("/v1/threads", {"agent_id": "instance-test", "title": "reuse",
                "runtime": {"mode": "reuse", "instance_id": first["runtime_instance_id"]}}, expected=201)
            assert reused["workdir"] == first["workdir"]
            reuse_task = task("reuse", "read", first_task["receipt"], first_task["workdir"], versions[0])
            reused = wait_turn(reused, submit(reused, reuse_task), reuse_task)
            assert reused["sandbox_id"] == first["sandbox_id"]
            verify_file(reused, reuse_task)
            # Hold one model request while a sibling queues on the same instance;
            # a different instance must remain executable.
            held_task = task("held", "read", first_task["receipt"], first_task["workdir"], versions[0], ["first"], True)
            held = submit(first, held_task)
            assert fixture.hold_started.wait(15)
            shared_task = task("shared", "read", first_task["receipt"], first_task["workdir"], versions[0], ["reuse"])
            shared = submit(reused, shared_task)
            time.sleep(0.2)
            with fixture.lock:
                assert "shared" not in fixture.started_labels, "same instance executed concurrently"
            independent_task = task("independent", "read", second_task["receipt"], second_task["workdir"], versions[1], ["second"])
            second = wait_turn(second, submit(second, independent_task), independent_task)
            fixture.hold_release.set()
            first = wait_turn(first, held, held_task)
            reused = wait_turn(reused, shared, shared_task)
            assert first["sandbox_id"] == reused["sandbox_id"]
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=45) == 0
            process = launch()
            assert len(api("/v1/agents/instance-test/instances")["instances"]) == 2
            restored_task = task("restored", "read", first_task["receipt"], first_task["workdir"], versions[0], ["reuse", "shared"])
            restored = wait_turn(reused, submit(reused, restored_task), restored_task)
            assert restored["sandbox_id"] == first["sandbox_id"]
            verify_file(restored, restored_task)
            if not fixture.docker:
                assert fixture.created == images + images
                assert len(fixture.deleted) == 2
                assert "/projects/default" in fixture.command_workdirs
                assert "/projects/custom project" in fixture.command_workdirs
            persisted = b"".join(path.read_bytes() for path in (directory / "state").glob("state.sqlite*"))
            for credential in [token, "instance-model-key", "instance-runtime-key"]:
                assert credential.encode() not in persisted
            assert fixture.completed_labels == {"first", "second", "reuse", "held", "shared", "independent", "restored"}
            report = {"result": "passed", "runtime": "real Docker" if fixture.docker else "AgentENV wire fixture",
                "mode": args.mode, "images": images, "instances": 2, "threads": 3, "turns": 7,
                "custom_default_workdir": True, "override_workdir": True, "reuse_files": True,
                "fresh_reuse_history": True, "shared_serialized": True, "independent_parallel": True,
                "restart": True, "per_image_mcp_catalog": fixture.docker,
                "elapsed_seconds": round(time.monotonic() - started, 3)}
        finally:
            original_error = sys.exc_info()[1]
            cleanup_errors = []
            fixture.hold_release.set()
            try:
                if process and process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    try:
                        process.wait(timeout=45)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
            database = directory / "state/state.sqlite"
            identifiers = set()
            if database.exists():
                try:
                    with sqlite3.connect(database) as db:
                        identifiers = {row[0] for row in db.execute("SELECT sandbox_id FROM runtime_instances WHERE sandbox_id IS NOT NULL")}
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
            for identifier in identifiers:
                try:
                    if fixture.docker:
                        try:
                            info = json.loads(docker("inspect", identifier))[0]
                        except subprocess.CalledProcessError as error:
                            if b"No such object" in error.stderr or b"No such container" in error.stderr:
                                continue
                            raise
                        labels = info["Config"]["Labels"]
                        assert labels["io.aporto.managed"] == "true"
                        assert labels["io.aporto.owner"] == "aporto-" + release_id[7:]
                        docker("rm", "--force", identifier)
                    else:
                        request = Request(endpoint + "/sandboxes/" + identifier,
                            headers={"X-API-Key": "instance-runtime-key"}, method="DELETE")
                        assert HTTP.open(request, timeout=10).status == 204
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
            fixture.shutdown()
            fixture.server_close()
            if log:
                log.close()
            if cleanup_errors:
                failure = RuntimeError("test-owned instance cleanup failed: " + ", ".join(cleanup_errors))
                if original_error is None:
                    raise failure
                if hasattr(original_error, "add_note"):
                    original_error.add_note(str(failure))
    print(json.dumps(report))


if __name__ == "__main__":
    main()
