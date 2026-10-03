#!/usr/bin/env bash
# A short session for the README screenshot, with the test service standing in for real programs.
# Usage: cargo build --release --examples && scripts/demo.sh
set -euo pipefail
cd "$(dirname "$0")/.."
T=$(cd target/release/examples && pwd -W 2>/dev/null || pwd)/testsvc
D=target/demo
rm -rf "$D" && mkdir -p "$D"
cat > "$D/tend.toml" <<TOML
control = "127.0.0.1:7399"
[service.db]
command = ["$T", "--ready-after", "300", "--listen", "127.0.0.1:7398"]
ready = { tcp = "127.0.0.1:7398" }
[service.api]
command = ["$T", "--say", "api listening on :8000"]
depends_on = ["db"]
ready = { log = "listening" }
[service.worker]
command = ["$T", "--fail-until-restarts", "2", "--say", "worker up"]
depends_on = ["db"]
backoff_initial_ms = 100
[service.web]
command = ["$T", "--ready-after", "150"]
depends_on = ["api"]
ready = { log = "ready" }
TOML
B=target/release/tend
echo "\$ tend run"
"$B" -c "$D/tend.toml" run > "$D/run.out" 2>&1 &
sleep 2
cat "$D/run.out"
echo
echo "\$ tend status"
"$B" -c "$D/tend.toml" status
echo
echo "\$ tend shutdown"
"$B" -c "$D/tend.toml" shutdown > /dev/null
wait
sed -n '/shutting down/,$p' "$D/run.out"
