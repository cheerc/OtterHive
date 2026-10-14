#!/bin/sh
# spawns_child.sh：在同一 process group 留下子程序（T3 建立）。
#
# 行為：在背景啟動 `sleep 600`（同一 process group），把子程序 pid 寫入
# `$OTTERHIVE_FIXTURE_LOG`，然後等待；設了 `FIXTURE_LEADER_EXIT_AFTER_MS`
# 就在該毫秒數後自己結束，留下子程序。`trap '' HUP` 讓子程序繼承忽略
# SIGHUP，避免非預期的 SIGHUP 破壞 leader-exit 殘留情境。
trap '' HUP
log="${OTTERHIVE_FIXTURE_LOG:-/dev/null}"
sleep 600 &
child=$!
printf '%s\n' "$child" >>"$log"
if [ -n "$FIXTURE_LEADER_EXIT_AFTER_MS" ]; then
    ms="$FIXTURE_LEADER_EXIT_AFTER_MS"
    secs="$(awk -v m="$ms" 'BEGIN { printf "%.3f", m / 1000 }')"
    sleep "$secs"
    exit 0
fi
wait
