#!/bin/sh
# non_reader.sh：**不讀 stdin** 的 backend（T5 建立）。
#
# 行為：啟動後完全不讀 PTY 的輸入，也永遠不輸出。
#
# 用途：驗證一個**不讀 stdin 的收件人**不會讓 daemon 對其他呼叫端失效
# （§6.4 的延後沒有上限，所以這樣的收件人會讓重判迴圈持續重判）。
#
# **這不是 D-1（阻塞式 PTY 寫入持有全域鎖）的迴歸防護，而且證明不了那件事**：
# 實測在讀端不動時，macOS PTY 的寫端接受 64 MiB 都沒有阻塞（見 PR 說明的
# 量測）。`Arc<Session>` 的改動仍值得保留（讓寫入不持鎖成為型別保證），只是
# 它防的不是一個會發生的問題。
#
# 收到 SIGTERM 就結束（daemon 的停止路徑需要它能乾淨退出）。
trap 'exit 0' TERM INT
# `sleep` 在前景，沒有 `&`——這個行程佔住 PTY 的 session leader 位置（§4.2-3
# 要求 `pgid == pid`），而 stdin 完全沒有人讀。
exec sleep 600