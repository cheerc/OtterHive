#!/bin/sh
# ignore_term.sh：忽略 SIGTERM（T3 建立）。stop 必須升級到 SIGKILL 才回 Stopped。
trap '' TERM
printf 'ignore-term-ready\n'
while true; do
    sleep 60
done
