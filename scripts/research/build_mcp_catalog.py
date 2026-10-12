#!/usr/bin/env python3
"""Build docs/reverse-engineering/mcp-catalog.json from the legacy tool schemas.

Usage:
    python3 -I scripts/research/build_mcp_catalog.py \
        --legacy-tools /path/to/agend-terminal/src/mcp/tools.rs \
        --legacy-commit <full sha> \
        --out docs/reverse-engineering/mcp-catalog.json

The legacy source is cheerc/agend-terminal on GitHub; check out the commit
passed as --legacy-commit (the published catalog uses
593da97c6f94d874f793777b4550d5df2ae4f43c) and point --legacy-tools at its
src/mcp/tools.rs.

The legacy schemas are `serde_json::json!` literals. They are parsed as JSON
after removing trailing commas; a literal that cannot be parsed (for example
one built with `format!`) must have an entry in MANUAL_SCHEMAS, otherwise the
script fails instead of guessing.
"""

import argparse
import json
import re
import sys
from pathlib import Path

PHASES = {
    "P1a": [
        "create_instance", "list_instances", "delete_instance", "start_instance",
        "restart_instance", "send", "inbox", "team", "deployment", "interrupt",
        "pane_snapshot",
    ],
    "P1b": ["instance", "move_pane", "set_metadata", "set_waiting_on"],
    "P2": ["task", "decision", "repo", "bind_self", "binding_state", "release_worktree"],
    "P3": ["ci", "revoke_review_assignment", "correct_review_class"],
    "P4": ["config", "health", "schedule", "restart_daemon", "usage_limit_takeover"],
    "deferred-operator": ["set_model"],
    "deferred-channel": ["reply", "operator_page", "download_attachment", "bind_topic"],
}

# Tools the agend-customization workflow calls, per
# skills/general/agend-changelog/references/dependency-surface.md:10 (d49735d).
CUSTOMIZATION_CALLS = {
    "send", "task", "inbox", "reply", "repo", "ci", "schedule", "restart_instance",
    "release_worktree", "deployment", "team", "decision", "health", "set_waiting_on",
    "interrupt", "pane_snapshot", "set_metadata", "binding_state", "list_instances",
    "correct_review_class",
}

# config's description is built with format!(), so its literal is not JSON.
MANUAL_SCHEMAS = {
    "config": {
        "name": "config",
        "inputSchema": {
            "type": "object",
            "properties": {"action": {"enum": ["get", "list"]}, "key": {}},
            "required": ["action"],
        },
    },
}

# P1a field support. Every legacy property not listed under `supported` is
# refused with `unsupported_in_phase` before any mutation.
P1A_SUPPORT = {
    "create_instance": {
        "supported": ["name", "backend", "args", "model", "effort", "working_directory",
                      "description", "role", "env"],
        "added": ["command", "instructions", "skills_path"],
        "supported_values": {"backend": ["claude", "command"]},
        "caller": "operator",
    },
    "list_instances": {"supported": ["instance"], "caller": "all callers"},
    "delete_instance": {"supported": ["instance"], "caller": "operator"},
    "start_instance": {"supported": ["instance"], "caller": "operator"},
    "restart_instance": {
        "supported": ["instance", "mode", "reason"],
        "added": ["expected_generation"],
        "supported_values": {"mode": ["fresh", "resume"]},
        "caller": "operator, manager; any instance for itself",
    },
    "interrupt": {"supported": ["instance"], "caller": "operator, manager; orchestrator for its team members"},
    "pane_snapshot": {"supported": ["instance", "lines"],
                      "caller": "operator, manager; orchestrator for its team; instance for itself"},
    "send": {
        "supported": ["instance", "instances", "team", "message", "request_kind",
                      "correlation_id", "parent_id", "thread_id"],
        "supported_values": {"request_kind": ["query", "report", "update"]},
        "caller": "operator, manager, instance (instance recipients follow the team ACL; sender identity is server-derived)",
    },
    "inbox": {
        "supported": ["action", "message_id"],
        "supported_actions": ["drain (action omitted)", "ack"],
        "added": ["delivery_token"],
        "caller": "instance for its own inbox",
    },
    "team": {
        "supported": ["action", "name", "members", "orchestrator", "description",
                      "add", "remove", "accept_from"],
        "supported_actions": ["create", "delete", "list", "update"],
        "caller": "operator; list: all callers",
    },
    "deployment": {
        "supported": ["action", "template", "name", "directory"],
        "supported_actions": ["deploy", "teardown", "list"],
        "caller": "operator",
    },
}

CONTRACT_CHANGES = {
    "inbox": [
        "drain issues a delivery_token and never acknowledges an earlier batch implicitly",
        "ack requires message_id plus the current delivery_token; stale or never-delivered rows are refused",
    ],
    "send": [
        "each recipient receives a durable message_id and its own outcome; a partial broadcast is reported as partial",
        "request_kind=task and every task/review/branch/CI field is refused until the phase that owns it",
        "the sender identity comes from the bridge credential, never from caller arguments",
    ],
    "restart_instance": [
        "resume without backend session evidence returns resume_unavailable and leaves the running process untouched",
        "the replacement starts only after the previous process is proven stopped; otherwise recovery_required",
    ],
    "create_instance": ["instance callers are refused; creation is an operator action"],
    "delete_instance": ["the inbox is archived under the deleted identity; a later instance with the same name gets a new identity"],
    "deployment": ["deploy reports each member's outcome: all succeed is completed, mixed is partial, all fail is refused; failed definitions are retained and never reported as deployed"],
}


def extract(src: str) -> dict:
    found = {}
    for m in re.finditer(r'json!\(\{\s*"name":\s*"([^"]+)"', src):
        name = m.group(1)
        start = src.index("{", m.start())
        depth, i, in_str, esc = 0, start, False, False
        while True:
            c = src[i]
            if in_str:
                if esc:
                    esc = False
                elif c == "\\":
                    esc = True
                elif c == '"':
                    in_str = False
            elif c == '"':
                in_str = True
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        lines = [src.count("\n", 0, m.start()) + 1, src.count("\n", 0, i) + 1]
        try:
            schema = json.loads(re.sub(r",(\s*[}\]])", r"\1", src[start:i + 1]))
            how = "parsed"
        except json.JSONDecodeError:
            if name not in MANUAL_SCHEMAS:
                sys.exit(f"cannot parse legacy schema for {name} and no manual entry exists")
            schema = MANUAL_SCHEMAS[name]
            how = "manual"
        found[name] = {"schema": schema, "lines": lines, "extraction": how}
    return found


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--legacy-tools", required=True)
    ap.add_argument("--legacy-commit", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    legacy = extract(Path(args.legacy_tools).read_text())
    phase_of = {tool: phase for phase, tools in PHASES.items() for tool in tools}
    missing = sorted(set(legacy) ^ set(phase_of))
    if missing:
        sys.exit(f"phase table and legacy tools disagree: {missing}")

    tools = []
    for name, item in legacy.items():
        schema = item["schema"].get("inputSchema", {})
        props = list(schema.get("properties", {}).keys())
        actions = schema.get("properties", {}).get("action", {}).get("enum")
        entry = {
            "name": name,
            "phase": phase_of[name],
            "otterhive_status": "planned",
            "called_by_customization": name in CUSTOMIZATION_CALLS,
            "legacy": {
                "source": f"agend-terminal@{args.legacy_commit}:src/mcp/tools.rs",
                "lines": item["lines"],
                "extraction": item["extraction"],
                "actions": actions,
                "properties": props,
                "required": schema.get("required", []),
            },
        }
        support = P1A_SUPPORT.get(name)
        if support:
            unknown = set(support["supported"]) - set(props)
            if unknown:
                sys.exit(f"{name}: supported fields not in legacy schema: {sorted(unknown)}")
            entry["p1a"] = {
                "caller": support["caller"],
                "supported_fields": support["supported"],
                "refused_fields": [p for p in props if p not in support["supported"]],
                "added_fields": support.get("added", []),
                "supported_actions": support.get("supported_actions"),
                "supported_values": support.get("supported_values", {}),
                "contract_changes": CONTRACT_CHANGES.get(name, []),
            }
        tools.append(entry)

    catalog = {
        "schema_version": 1,
        "legacy_source": {"repository": "cheerc/agend-terminal", "commit": args.legacy_commit,
                          "file": "src/mcp/tools.rs"},
        "customization_source": {
            "repository": "agend-customization",
            "commit": "d49735d89ae97c58a09cc243428dd629d013a650",
            "file": "skills/general/agend-changelog/references/dependency-surface.md:10",
        },
        "phases": list(PHASES.keys()),
        "tool_count": len(tools),
        "tools": tools,
    }
    Path(args.out).write_text(json.dumps(catalog, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
