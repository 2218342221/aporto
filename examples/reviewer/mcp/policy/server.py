"""Dependency-free example MCP server; runs inside the AgentENV guest."""
import json
import pathlib
import sys


def dispatch(request):
    method = request["method"]
    if method == "initialize":
        return {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                "serverInfo": {"name": "packaged-review-policy", "version": "1.0.0"}}
    if method == "tools/list":
        return {"tools": [{"name": "review_policy", "description": "Read the packaged review policy.",
                           "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}]}
    if method == "tools/call" and request.get("params", {}).get("name") == "review_policy":
        policy = pathlib.Path(__file__).with_name("policy.json").read_text()
        return {"content": [{"type": "text", "text": policy}], "isError": False}
    if method == "ping":
        return {}
    raise ValueError("Unsupported method")


for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    response = {"jsonrpc": "2.0", "id": request["id"]}
    try:
        response["result"] = dispatch(request)
    except Exception:
        response["error"] = {"code": -32601, "message": "Unsupported request"}
    print(json.dumps(response), flush=True)
