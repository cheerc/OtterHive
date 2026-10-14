#!/bin/sh
# echo_agent.sh：逐行回應的 PTY fixture（T3 建立）。
#
# 行為：印出 `ready>`；逐行讀 stdin，把每行的原始 bytes（十六進位）附加到
# `$OTTERHIVE_FIXTURE_LOG`，並回印 `echo: <行>`；收到 `size` 回 `stty size`；
# 收到 `flood` 在背景持續輸出直到收到 `stop`（主迴圈仍可讀 stdin，不用無法
# 讀 stdin 的輸出迴圈）。
log="${OTTERHIVE_FIXTURE_LOG:-/dev/null}"
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
    printf '%s' "$line" | od -An -tx1 | tr -d ' \n' >>"$log"
    printf '\n' >>"$log"
    case "$line" in
        size)
            stty size
            ;;
        flood)
            if [ -z "$flood_pid" ]; then
                i=0
                while true; do
                    i=$((i + 1))
                    printf 'flood-output-line %d\n' "$i"
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
