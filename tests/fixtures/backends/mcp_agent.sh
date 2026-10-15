#!/bin/sh
# mcp_agent.sh：`mcp_agent.py` 的啟動包裝（T5 建立）。
#
# 為什麼需要它：契約 §5.1 的 `claude` backend 只把 `command` 當**可執行檔的覆寫**
# （§4.1：「`command` 可以是絕對路徑」，§5.1 的 argv 仍照 claude 的形狀組成），
# 而 §5.2 的 `backend: command` 才是「只執行 `command` 加 `args`、不加任何參數」
# ——但 `backend: command` 在 P1a 回 `unsupported_in_phase`（plan T4）。
#
# 所以 `python3` 會直接看到 `--dangerously-skip-permissions`、
# `--append-system-prompt-file <path>`、`--mcp-config <path>`：python 在**腳本路徑
# 之前**解析自己的選項，遇到未知旗標就 `Unknown option` 並以 exit 2 結束
# （`create_instance` 因此回 `spawn_failed`／`exited_during_startup`）。這不是
# fixture 的問題，是 argv 形狀與 P1a 只有 `claude` 一種 backend 的必然結果。
#
# 這支 wrapper 的做法：自己 exec `python3 -I <script>`，把 argv 完全交給自己組。
# agent 需要的 bridge 路徑與 token 都來自 §5.3 的**環境變數**，不需要 argv，
# 而「不啟動真 Claude CLI」正是本 fixture 的前提。
#
# §plan：fixture 以 `python3 -I` 執行；`bridge` 路徑與 token 從 `OTTERHIVE_MCP_BRIDGE`／
# `OTTERHIVE_INSTANCE_TOKEN` 取得。兩者都不依賴 PATH 上的 `python3`（預設用
# `/usr/bin/python3`，可用 `OTTERHIVE_TEST_PYTHON` 覆寫成別的絕對路徑）。

python="${OTTERHIVE_TEST_PYTHON:-/usr/bin/python3}"
self_dir="$(dirname "$0")"
exec "$python" -I "$self_dir/mcp_agent.py" "$@"
