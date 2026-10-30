#!/usr/bin/env bash
# A guided tour of every testing technique detvisor demonstrates.
# Usage: ./demo.sh            (takes about two minutes)
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release -q --features turso 2>/dev/null || cargo build --release -q
B=${CARGO_TARGET_DIR:-target}/release/detvisor
rm -rf out
step() { printf '\n\033[1;36m==> %s\033[0m\n$ %s\n' "$1" "$2"; eval "$2"; }

step "1. No faults: correctness passes, but Sometimes properties reveal untested situations" \
  "$B run --runs 100 --faults none | tail -22"
step "2. Faults on, correct app: 300 timelines of crashes, partitions, pauses... all properties pass" \
  "$B run --runs 300 | sed -n '3,6p'"
step "3. Concurrency bug found by exploring thread interleavings" \
  "$B run --runs 100 --bugs race --stop-on-fail | grep -E 'FAIL \"|Money'"
step "4. Durability bug found by crash faults" \
  "$B run --runs 1000 --bugs ack --stop-on-fail | grep -E 'FAIL \"|Acknowledged'"
step "5. Idempotency bug found by packet loss/duplication + retries" \
  "$B run --runs 100 --bugs dup --stop-on-fail | grep -E 'FAIL \"|at most once'"
step "6. Exact replay of a failing timeline (and a determinism self-check)" \
  "$B replay out/acknowledged-transfers-survive-crashes-1.tape --grep FAULT --tail 8"
step "7. Shrink the failing timeline to its essence" \
  "$B shrink out/acknowledged-transfers-survive-crashes-1.tape"
step "8. A rare deep bug: random vs guided exploration from sometimes-assertion checkpoints" \
  "$B compare --bugs compact --trials 4 --budget-s 1500"
step "9. Find it once with guided exploration and save the tape" \
  "$B run --runs 20000 --bugs compact --strategy guided --stop-on-fail | head -2"
T=out/recovery-finds-a-snapshot-that-covers-the-truncated-log-1.tape
step "10. Causality analysis: when did the bug become inevitable?" \
  "$B causality $T --points 20 --futures 30"
step "11. Multiverse debugging: the machine just before and just after the fatal crash, 200 alternate futures each" \
  "$B inspect $T --event 'KILL' --futures 200"
step "12. The leaky boat: one uncontrolled source of nondeterminism breaks replay" \
  "$B check-determinism --runs 3 --bugs leak | tail -4"
step "13. Disk faults: torn writes are survived thanks to checksums; the torn bug crashes recovery" \
  "$B run --runs 400 | grep -E 'torn|faults inj'; $B run --runs 1000 --bugs torn --stop-on-fail | grep 'timeline '"
step "14. Opt-in disk faults: bit rot and lying fsync break a single-node system" \
  "$B run --runs 300 --enable corrupt | grep -E 'FAIL'; $B run --runs 300 --enable lying_fsync | grep -E 'FAIL'"
step "15. A replicated system: 3-node Raft, correct under 300 faulted timelines" \
  "$B run --app cluster --runs 300 | sed -n '/Always properties/,\$p'"
step "16. Consensus bugs" \
  "for b in no_quorum vote_twice log_check; do $B run --app cluster --runs 2000 --bugs \$b --stop-on-fail | grep 'timeline '; done"
step "17. Swarm testing + BUGGIFY" \
  "$B run --runs 300 --swarm --buggify --bugs ack | grep -E 'swarm=|timelines:|faults inj|FAIL'"
if $B knobs --app turso >/dev/null 2>&1; then
  step "18. Real Turso under detvisor (built with --features turso)" \
    "$B run --app turso --runs 100 | sed -n '/Always properties/,/Sometimes properties/p'"
  step "19. Turso app-level bugs" \
    "for b in ack_early nosync sync_off rm_wal; do $B run --app turso --runs 300 --bugs \$b --stop-on-fail | grep 'timeline '; done"
fi
