"""Minimal MCP server (stdio, JSON-RPC 2.0) exposing Python standard-library documentation.

Standard library only; it imports nothing but standard-library modules, so a model cannot
make it load arbitrary code. Tools:
  lookup(name)          documentation of a module, class or function, e.g. "unittest.TestCase"
  module_members(name)  public names defined by a standard-library module
"""

import importlib
import json
import pydoc
import sys

MAX_CHARS = 6000

TOOLS = [
    {
        "name": "lookup",
        "description": "Documentation of a Python standard-library module, class, function or "
        "method, e.g. 'unicodedata.normalize' or 'unittest.TestCase.assertRaises'.",
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Dotted name."}},
            "required": ["name"],
        },
    },
    {
        "name": "module_members",
        "description": "Public names defined by a Python standard-library module, e.g. 're'.",
        "inputSchema": {
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Module name."}},
            "required": ["name"],
        },
    },
]


def stdlib_object(dotted):
    """Resolves a dotted name, importing only standard-library modules."""
    parts = dotted.strip().split(".")
    if not parts or not parts[0] or parts[0] not in sys.stdlib_module_names:
        raise ValueError(f"'{dotted}' is not in the Python standard library")
    obj = importlib.import_module(parts[0])
    for index, part in enumerate(parts[1:], start=1):
        if hasattr(obj, part):
            obj = getattr(obj, part)
        else:
            obj = importlib.import_module(".".join(parts[: index + 1]))
    return obj


def lookup(name):
    text = pydoc.render_doc(stdlib_object(name), renderer=pydoc.plaintext)
    return text[:MAX_CHARS] + ("\n[... truncated ...]" if len(text) > MAX_CHARS else "")


def module_members(name):
    module = stdlib_object(name)
    names = getattr(module, "__all__", None) or [n for n in dir(module) if not n.startswith("_")]
    return ", ".join(sorted(names))


def handle(message):
    method = message.get("method")
    params = message.get("params") or {}
    if method == "initialize":
        return {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pydoc", "version": "1.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        tool = params.get("name")
        arguments = params.get("arguments") or {}
        try:
            if tool == "lookup":
                text = lookup(arguments["name"])
            elif tool == "module_members":
                text = module_members(arguments["name"])
            else:
                return {"content": [{"type": "text", "text": f"unknown tool {tool}"}], "isError": True}
            return {"content": [{"type": "text", "text": text}], "isError": False}
        except Exception as error:  # reported to the model as a tool error
            return {"content": [{"type": "text", "text": f"{type(error).__name__}: {error}"}], "isError": True}
    if method == "ping":
        return {}
    raise LookupError(method)


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "id" not in message:
            continue  # notification
        try:
            reply = {"jsonrpc": "2.0", "id": message["id"], "result": handle(message)}
        except LookupError:
            reply = {"jsonrpc": "2.0", "id": message["id"],
                     "error": {"code": -32601, "message": f"method not found: {message.get('method')}"}}
        sys.stdout.write(json.dumps(reply) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
