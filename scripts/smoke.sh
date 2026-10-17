#!/usr/bin/env bash
# End-to-end smoke test of `vramorama run` on the real GPU (needs the card admitting leases).
# usage: scripts/smoke.sh [path to vramorama.exe] [llama-perplexity.exe] [small .gguf]
# The VRAM-enforcement case needs a model that holds well over its lease, e.g. a 1 GB+ GGUF.
V=${1:-vramorama}; PERP=${2:-}; MODEL=${3:-}
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got $2, want $3"; fail=$((fail+1)); fi; }

$V run --vram 64M --quiet --wait 86400 -- cmd /c "exit 7";                      check "exit code passes through" $? 7
$V run --vram 64M --quiet --wait 86400 --restart 2 -- cmd /c "exit 3";          check "plain failure is not retried (no GPU reset logged)" $? 3
$V run --vram 64M --quiet --wait 86400 --guard-power 60 -- cmd /c "exit 0";     check "guard flags leave a healthy job alone" $? 0
if [ -n "$PERP" ] && [ -n "$MODEL" ]; then
  $V run --vram 256M --quiet --wait 86400 --enforce-vram 128 -- "$PERP" -m "$MODEL" -f README.md -c 128 -ngl 999 > /dev/null 2>&1
  check "a job over its lease is stopped (exit 3)" $? 3
fi
echo "$pass passed, $fail failed"
[ $fail -eq 0 ]
