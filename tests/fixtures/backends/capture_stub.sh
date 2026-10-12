#!/bin/sh
# Deterministic stand-in for a backend CLI: prints a ready marker plus CJK and
# ANSI-styled text, then exits. Used by scripts/research/rea-baseline.sh.
printf 'otterhive-fixture-ready\n'
printf '中文 寬字元 \033[1;32mANSI\033[0m\n'
