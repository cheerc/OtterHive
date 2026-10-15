#!/bin/sh
# non_reader.sh：**不讀 stdin** 的 backend（T5 建立）。
#
# 行為：啟動後完全不讀 PTY 的輸入，也永遠不輸出。
#
# 用途：驗證 §6.4「延後沒有上限」之下，daemon 對**其他**呼叫端仍然可用——一個
# 不讀 stdin 的 agent 不該讓 `list_instances`／`pane_snapshot`／attach 停擺。
#
# 這不是 D-1（阻塞式 PTY 寫入持有全域鎖）的迴歸防護，而且**證明不了**那件事：
# 實測在讀端不動時，macOS PTY 的寫端接受 64 MiB 都沒有阻塞（見 PR 說明的
# 量測）。所以「寫入阻塞 → 鎖死 daemon」沒有可達路徑，D-1 防的是一個不可達的
# 威脅。`Arc<Session>` 的改動本身仍是對的（讓寫入不持鎖成為型別保證），只是它
# 防的不是一個會發生的問題。
#
# 收到 SIGTERM 就結束（daemon 的停止路徑需要它能乾淨退出）。
trap 'exit 0' TERM INT
# `sleep` 在前景，沒有 `&`——這個行程佔住 PTY 的 session leader 位置（§4.2-3
# 要求 `pgid == pid`），而 stdin 完全沒有人讀。
exec sleep 600