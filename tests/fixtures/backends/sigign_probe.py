#!/usr/bin/env python3
"""sigign_probe.py：在隔離 helper 內驗證 SIGCHLD=SIG_IGN 自動回收（T3 RED）。

以 `python3 -I` 執行。helper 設 SIG_IGN 後 spawn `sleep 0.2`，等待後以
WNOHANG waitpid：自動回收時報 ECHILD（errno 10），印出 ECHILD_AFTER_IGN；
否則印出 WAITED。不動父行程（測試 runner）的 disposition。
只用於 disposition 前提驗證，不參與產品邏輯。
"""
import os
import signal
import subprocess
import sys
import time


def main() -> int:
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)
    proc = subprocess.Popen(["sleep", "0.2"])
    time.sleep(0.6)
    try:
        pid, status = os.waitpid(proc.pid, os.WNOHANG)
        print(f"WAITED pid={pid} status={status}")
        return 1
    except ChildProcessError as exc:
        print(f"ECHILD_AFTER_IGN errno={exc.errno}")
        return 0


if __name__ == "__main__":
    sys.exit(main())
