"""Standard-library MCP server exposing packaged software engineering guidance."""

import json
from pathlib import Path
import sys


TOOL = {
    "name": "engineering_guidelines",
    "description": "Read the packaged implementation, validation, and patch-delivery guidelines.",
    "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
}


class RpcError(Exception):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def dispatch(request):
    method = request["method"]
    if method == "initialize":
        return {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "packaged-engineering-guidelines", "version": "1.0.0"},
        }
    if method == "ping":
        return {}
    if method == "tools/list":
        return {"tools": [TOOL]}
    if method == "tools/call":
        params = request.get("params", {})
        if not isinstance(params, dict) or params.get("name") != TOOL["name"]:
            raise RpcError(-32602, "Unknown tool")
        if params.get("arguments", {}) != {}:
            raise RpcError(-32602, "engineering_guidelines takes no arguments")
        guidance = Path(__file__).with_name("guidelines.json").read_text(encoding="utf-8")
        return {"content": [{"type": "text", "text": guidance}], "isError": False}
    raise RpcError(-32601, "Method not found")


def serve():
    for line in sys.stdin:
        response = {"jsonrpc": "2.0", "id": None}
        try:
            request = json.loads(line)
            if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
                raise RpcError(-32600, "Invalid request")
            if not isinstance(request.get("method"), str):
                raise RpcError(-32600, "Invalid request")
            if "id" not in request:
                continue
            response["id"] = request["id"]
            response["result"] = dispatch(request)
        except json.JSONDecodeError:
            response["error"] = {"code": -32700, "message": "Parse error"}
        except RpcError as error:
            response["error"] = {"code": error.code, "message": str(error)}
        except OSError:
            response["error"] = {"code": -32603, "message": "Packaged guidelines are unavailable"}
        print(json.dumps(response), flush=True)


if __name__ == "__main__":
    serve()
