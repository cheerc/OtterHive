#!/usr/bin/env python3
# mcp_agent.py：會用真 bridge 協作的 PTY fixture（T5 建立）。
#
# 依 plan（docs/plans/p1a-implementation.md 的 fixture 表）與契約 §5.3：
# - 以 `python3 -I` 執行；從環境變數讀 `OTTERHIVE_MCP_BRIDGE`（bridge 的絕對
#   路徑）與 `OTTERHIVE_INSTANCE_TOKEN`（§5.1 寫入 MCP 設定的同一組 token）。
# - 讀 PTY 輸入；看到 `[OTTERHIVE-MSG]` 提示（§6.4）就啟動**真的**
#   `otterhive-mcp-bridge`（stdio JSON-RPC），走 `initialize` →
#   `tools/call inbox`（drain）→ 逐則以 `delivery_token` ack。
# - 訊息內容為 `relay:<target>:<text>` 時，送 `request_kind: query` 的
#   `<text>` 給 `<target>` 並帶 `parent_id`；收到 query 時回 `report`
#   `pong:<text>` 給 sender 並帶 `parent_id`。
# - 所有收到的訊息與自己的 pid 寫入 `$OTTERHIVE_FIXTURE_LOG`（一行一個 JSON）。
#
# 這支 fixture **不啟動真 Claude CLI**：bridge 是真的，但被協作的是我們自己
# 的協定（JSON-RPC 與 daemon 的操作），不是 agent 的行為。

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading

# 提示裡會被記錄的 key（契約 §6.4 的 `from=`／`kind=`／`id=`）。
HINT_KEYS = ("from", "kind", "id")


def log(record: dict) -> None:
    """把一筆 JSON 記錄 append 到 `$OTTERHIVE_FIXTURE_LOG`。

    寫檔失敗只印到 stderr：log 是測試觀察用的，不該讓 fixture 為它崩潰。
    """
    path = os.environ.get("OTTERHIVE_FIXTURE_LOG")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, ensure_ascii=False) + "\n")
            handle.flush()
    except OSError as error:
        print(f"mcp_agent: 無法寫入 log：{error}", file=sys.stderr)


def bridge_call(method: str, params: dict = None, request_id: int = 1) -> dict:
    """啟動一次真 bridge，只送一則 JSON-RPC 請求就結束。

    每則訊息處理都起一個新的 bridge：契約 §10 只要求 `initialize` 前置，
    而我們要測的是「這則訊息有沒有被正確投遞與 ack」，不是 bridge 的生命週期。
    """
    bridge = os.environ.get("OTTERHIVE_MCP_BRIDGE")
    if not bridge:
        log({"event": "bridge_missing", "method": method})
        return {}
    token = os.environ.get("OTTERHIVE_INSTANCE_TOKEN", "")
    request = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        request["params"] = params
    completed = subprocess.run(
        [bridge],
        input=(json.dumps(request) + "\n").encode("utf-8"),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env={**os.environ, "OTTERHIVE_INSTANCE_TOKEN": token},
    )
    for line in completed.stdout.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue
    log(
        {
            "event": "bridge_no_response",
            "method": method,
            "stderr": completed.stderr.decode("utf-8", "replace")[-400:],
        }
    )
    return {}


def call_tool(name: str, arguments: dict, request_id: int = 1) -> dict:
    """`tools/call` 的結果；把 `content[0].text` 的 JSON 解回 dict。"""
    response = bridge_call(
        "tools/call", {"name": name, "arguments": arguments}, request_id=request_id
    )
    if "result" not in response:
        log({"event": "tool_call_failed", "tool": name, "response": response})
        return {}
    content = response["result"].get("content") or [{}]
    text = content[0].get("text") or ""
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        log({"event": "tool_call_unparsable", "tool": name, "text": text})
        return {}


def send(target: str, body: str, kind: str, parent_id: str, request_id: int = 1) -> None:
    """以 `send` 送出一則訊息（sender 身分由 token 推導，§2 不接受自報）。"""
    arguments: dict = {"instance": target, "message": body, "request_kind": kind}
    if parent_id:
        arguments["parent_id"] = parent_id
    result = call_tool("send", arguments, request_id=request_id)
    log({"event": "sent", "target": target, "body": body, "result": result})


def handle_message(message: dict, delivery_token: str) -> None:
    """依 §6.1 的 `kind` 決定回覆動作，然後以本批 token ack。"""
    body = message.get("body") or ""
    sender = (message.get("from") or {}).get("name")
    parent_id = message.get("id")

    if body.startswith("relay:"):
        # relay:<target>:<text>：送 query 給 target，並帶 parent_id。
        _, target, text = body.split(":", 2)
        log({"event": "relay", "target": target, "text": text, "parent_id": parent_id})
        send(target, text, "query", parent_id, request_id=2)
    elif message.get("kind") == "query":
        # 收到 query：回 report `pong:<text>` 給 sender，帶 parent_id。
        log({"event": "pong", "text": body, "parent_id": parent_id})
        if sender:
            send(sender, "pong:" + body, "report", parent_id, request_id=2)

    # 每則都帶同一組 `delivery_token` ack（§6.3）。
    ack = call_tool(
        "inbox",
        {"action": "ack", "message_id": message["id"], "delivery_token": delivery_token},
        request_id=3,
    )
    log({"event": "acked", "message_id": message.get("id"), "result": ack})


def drain_and_ack(request_id: int = 1) -> None:
    """drain 自己的 inbox，逐則處理並 ack。"""
    drained = call_tool("inbox", {}, request_id=request_id)
    value = drained.get("value") or {}
    delivery_token = value.get("delivery_token")
    for message in value.get("messages") or []:
        log({"event": "received", "message": message})
        if delivery_token:
            handle_message(message, delivery_token)


def read_pty() -> None:
    """讀 PTY 直到 EOF；遇到喚醒提示就 drain 一次。"""
    for raw in sys.stdin:
        line = raw.replace("\r", "").strip()
        if not line:
            continue
        if line.startswith("[OTTERHIVE-MSG]"):
            hint = {}
            for token in line.split()[1:]:
                key, _, value = token.partition("=")
                if key in HINT_KEYS:
                    hint[key] = value
            log({"event": "hint", "hint": hint})
            drain_and_ack(request_id=1)


def main() -> None:
    """先記錄自己的 pid，再讀 PTY 直到 EOF。"""
    log(
        {
            "event": "started",
            "pid": os.getpid(),
            "instance": os.environ.get("OTTERHIVE_INSTANCE"),
            "has_bridge": bool(os.environ.get("OTTERHIVE_MCP_BRIDGE")),
            "has_token": bool(os.environ.get("OTTERHIVE_INSTANCE_TOKEN")),
        }
    )
    # fire-and-forget: PTY 讀取執行緒沒有可 join 的上界（daemon 關掉 PTY 才收到
    # EOF），所以是 daemon thread；程序結束時它會被一起收掉。
    reader = threading.Thread(target=read_pty, daemon=True)
    reader.start()
    reader.join()


if __name__ == "__main__":
    main()
