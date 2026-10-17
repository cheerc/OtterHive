#!/bin/sh
# non_reader.sh：**不讀 stdin** 的 backend（T5 建立）。
#
# 行為：啟動後完全不讀 PTY 的輸入，也永遠不輸出。
#
# 用途：驗證一個**不讀 stdin 的收件人**不會讓 daemon 對其他呼叫端失效
# （§6.4 的延後沒有上限，所以這樣的收件人會讓重判迴圈持續重判）。
#
# **這個阻塞已經證實可達**：daemon 曾兩次因此紅（CI），`sample` 取得的執行緒堆疊
# 顯示喚醒的重判執行緒卡在 `write(2)`。機制與處置見
# `docs/worklog-pty-write-block.md`：`write(2)` 在 master→slave 的輸入佇列滿時
# 阻塞，而那個佇列**只由子程序讀 stdin 才會排空**——所以本 fixture（不讀 stdin）
# 一定會踩到，且不會自己好。
#
# 處置已實作：`Session::write_input` 改用 `try_lock`（其他呼叫端不排隊），且
# `interrupt`／attach Input 的寫入移出 daemon 鎖。**`O_NONBLOCK` 那條路線已
# 否決**（dup 不隔離旗標、slave 方向送不到 child stdin、`write_all` 會切斷提示行）。
#
# 收到 SIGTERM 就結束（daemon 的停止路徑需要它能乾淨退出）。
trap 'exit 0' TERM INT
# `sleep` 在前景，沒有 `&`——這個行程佔住 PTY 的 session leader 位置（§4.2-3
# 要求 `pgid == pid`），而 stdin 完全沒有人讀。
exec sleep 600