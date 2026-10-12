#!/usr/bin/env bash
# Reproduce the legacy agend-terminal PTY/capture baseline with REA.
#
#   scripts/research/rea-baseline.sh /path/to/agend-terminal-binary [output.json]
#
# The legacy binary runs under `env -i` with a fresh AGEND_HOME, HOME and cwd,
# and a PATH whose only `claude` is tests/fixtures/backends/capture_stub.sh.
# It never reads ~/.agend and never starts a paid backend. The scenario data
# lives in a new temporary directory, separate from this script.
set -euo pipefail

legacy_bin=${1:?usage: rea-baseline.sh <agend-terminal binary> [output.json]}
repo_root=$(cd "$(dirname "$0")/../.." && pwd)
stub="$repo_root/tests/fixtures/backends/capture_stub.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/otterhive-rea.XXXXXX")
out=${2:-"$work/capture.json"}
mkdir -p "$work/fake-bin" "$work/agend-home" "$work/user-home" "$work/cwd"
cp "$stub" "$work/fake-bin/claude"
chmod 755 "$work/fake-bin/claude"

python3 -I - "$work" "$legacy_bin" > "$work/scenario.json" <<'EOF'
import json, sys
work, legacy = sys.argv[1], sys.argv[2]
print(json.dumps({
    "executable": "/usr/bin/env",
    "arguments": [
        "-i", f"AGEND_HOME={work}/agend-home", f"HOME={work}/user-home",
        "AGEND_CAPTURE_FIXTURES=1", f"PATH={work}/fake-bin:/usr/bin:/bin",
        "TERM=xterm-256color", legacy,
        "capture", "backend", "--backend", "claude", "--seconds", "1",
    ],
    "working_directory": f"{work}/cwd",
    "filesystem_observation_paths": [f"{work}/agend-home", f"{work}/user-home", f"{work}/cwd"],
    "terminal": {"columns": 100, "rows": 30, "scrollback": 200},
    "timeout_ms": 20000, "idle_timeout_ms": 10000, "finalization_ms": 1500, "settle_ms": 200,
    "limits": {"file_bytes": 2000000, "files": 200, "filesystem_depth": 6,
               "output_bytes": 65536, "processes": 64},
}))
EOF

rea_cmd=(rea)
command -v rea >/dev/null 2>&1 || rea_cmd=(npx -y rea-agents@6.4.0)
status=0
"${rea_cmd[@]}" capture-process "$work/scenario.json" --format json > "$out" || status=$?
printf 'scenario: %s\nevidence: %s\nrea exit: %s\n' "$work/scenario.json" "$out" "$status"
# A non-zero exit keeps REA's partial observation in $out. `cleanup_incomplete`
# can name an unrelated process that started during the window; inspect the
# reported PIDs before treating it as a leak from the scenario.
exit "$status"
