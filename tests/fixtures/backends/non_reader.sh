#!/bin/sh
# non_reader.sh：**不讀 stdin** 的 backend（T5 建立）。
#
# 行為：啟動後完全不讀 PTY 的輸入，也永遠不輸出。
#
# 用途：驗證一個**不讀 stdin 的收件人**不會讓 daemon 對其他呼叫端失效
# （§6.4 的延後沒有上限，所以這樣的收件人會讓重判迴圈持續重判）。
#
# **這個阻塞已經證實可達**：daemon 曾兩次因此紅（CI），`sample` 取得的執行緒堆疊
# 顯示喚醒的重判執行緒卡在 `write(2)`，而處理該次 `send` 的執行緒在等同一把
# `core.writer` 鎖。證據、累積門檻與已裁決的修法（writer 改 `O_NONBLOCK`）見
# `docs/worklog-t6-pty-write-block.md`。
#
# 這支 fixture 存在的原因就是它：**`Arc<Session>` 只解決了鎖的粒度**（寫入不持
# daemon 鎖），沒有解決 `write_input` 內部那把 `core.writer` 仍被佔住的問題。
#
# 收到 SIGTERM 就結束（daemon 的停止路徑需要它能乾淨退出）。
trap 'exit 0' TERM INT
# `sleep` 在前景，沒有 `&`——這個行程佔住 PTY 的 session leader 位置（§4.2-3
# 要求 `pgid == pid`），而 stdin 完全沒有人讀。
exec sleep 600