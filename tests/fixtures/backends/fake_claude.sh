#!/bin/sh
# fake_claude.sh：假 Claude CLI（T4 建立）。
#
# 行為（供 fake backend 驗證 §5.1／§5.3／§5.4）：
# - 把收到的 argv 逐行寫入 `$OTTERHIVE_FIXTURE_LOG`，之後行為同 `echo_agent.sh`
#   （印 `ready>`、逐行 echo、支援 `size`／`flood`）。
# - 把環境變數名稱（不 含值）寫入 `$OTTERHIVE_FIXTURE_ENV`，供測試確認
#   `OTTERHIVE_HOME`／`OTTERHIVE_INSTANCE`／`OTTERHIVE_INSTANCE_TOKEN`／
#   `OTTERHIVE_MCP_BRIDGE` 存在，且沒有 `OTTERHIVE_OPERATOR` 前綴的變數。
# - `--mcp-config <path>` 與 `--append-system-prompt-file <path>` 的內容
#   原樣寫入 `$OTTERHIVE_FIXTURE_MCP`／`$OTTERHIVE_FIXTURE_PROMPT`。
# - resume 證據（§5.4）：`<workdir>/.claude/projects/<編碼>/*.jsonl` 存在且含
#   `"type":"user"` 時，`$OTTERHIVE_FIXTURE_RESUME` 寫入 `evidence`；否則寫
#   `none`。

log="${OTTERHIVE_FIXTURE_LOG:-/dev/null}"
mcp_dump="${OTTERHIVE_FIXTURE_MCP:-/dev/null}"
prompt_dump="${OTTERHIVE_FIXTURE_PROMPT:-/dev/null}"
env_dump="${OTTERHIVE_FIXTURE_ENV:-/dev/null}"
term_dump="${OTTERHIVE_FIXTURE_TERM:-/dev/null}"
resume_dump="${OTTERHIVE_FIXTURE_RESUME:-/dev/null}"

: >"$log"

# argv 逐行寫入 log（測試以行序比對 §5.1 的參數順序）。
for arg in "$@"; do
    printf '%s\n' "$arg" >>"$log"
done

# 環境變數名稱（不含值）寫入 env dump；TERM 的實際值另存一份，供測試驗
# §5.3 要求的 `TERM=xterm-256color`（名稱 dump 看得到「有 TERM」但看不到值）。
env | sed 's/=.*//' | sort >"$env_dump"
printf '%s\n' "${TERM:-}" >"$term_dump"

# 取出 --mcp-config / --append-system-prompt-file 的檔案內容。
previous=""
for arg in "$@"; do
    case "$previous" in
        --mcp-config)
            if [ -f "$arg" ]; then
                cat "$arg" >"$mcp_dump"
            else
                : >"$mcp_dump"
            fi
            ;;
        --append-system-prompt-file)
            if [ -f "$arg" ]; then
                cat "$arg" >"$prompt_dump"
            else
                : >"$prompt_dump"
            fi
            ;;
    esac
    previous="$arg"
done

# resume 證據（§5.4）：編碼為 canonical workdir 中非 [A-Za-z0-9-] 換成 `-`。
workdir="$PWD"
encoded="$(printf '%s' "$workdir" | sed 's/[^A-Za-z0-9-]/-/g')"
resume="none"
if [ -d "${HOME:-}/.claude/projects/$encoded" ]; then
    if grep -rqs '"type":"user"' "${HOME:-}/.claude/projects/$encoded"/*.jsonl 2>/dev/null; then
        resume="evidence"
    fi
fi
printf '%s\n' "$resume" >"$resume_dump"

flood_pid=""
stop_flood() {
    if [ -n "$flood_pid" ]; then
        kill "$flood_pid" 2>/dev/null
        wait "$flood_pid" 2>/dev/null
        flood_pid=""
    fi
}
trap 'stop_flood; exit 0' EXIT HUP INT TERM
printf 'ready>\n'
while IFS= read -r line; do
    line="$(printf '%s' "$line" | tr -d '\r')"
    case "$line" in
        size)
            stty size
            ;;
        flood)
            if [ -z "$flood_pid" ]; then
                i=0
                while true; do
                    i=$((i + 1))
                    printf 'fake-claude-output-line %d\n' "$i"
                done &
                flood_pid=$!
            fi
            printf 'echo: flood\n'
            ;;
        stop)
            stop_flood
            printf 'echo: stop\n'
            ;;
        *)
            printf 'echo: %s\n' "$line"
            ;;
    esac
done
stop_flood
exit 0