#!/bin/sh
# ignore_term.sh：忽略 SIGTERM（T3 建立）。stop 必須升級到 SIGKILL 才回 Stopped。
# trap 生效後印就緒標記：測試等到該行才發 signal，避免 TERM 到達早於 trap 設置。
trap '' TERM
printf 'ignore-term-ready\n'
while true; do
    sleep 60
done
