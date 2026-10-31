#!/usr/bin/env bash
# One command for the whole verification of ryo-v2.
#
#   bash scripts/verify.sh            # just check: fmt, clippy -D warnings, test, deny
#   bash scripts/verify.sh oracle     # the oracle harness only (just oracle)
#   bash scripts/verify.sh all        # check, then oracle
#   bash scripts/verify.sh bench ARGS # ryo-v2 bench ARGS (e.g. corpus/base/extract-trait-multicrate corpus/broken-01)
#
# `just check` is the definition of green (CI runs the same recipe). The
# script exists so that one allowed command covers it, with the repo root fixed.
set -euo pipefail
cd "$(dirname "$0")/.."

mode="${1:-check}"
shift || true

has_recipe() { just --summary 2>/dev/null | tr ' ' '\n' | grep -qx "$1"; }

case "$mode" in
  check)
    just check
    ;;
  oracle)
    has_recipe oracle || { echo "$0: no 'oracle' recipe in Justfile yet" >&2; exit 2; }
    just oracle
    ;;
  all)
    just check
    if has_recipe oracle; then just oracle; else echo "$0: no 'oracle' recipe yet, skipped" >&2; fi
    ;;
  bench)
    cargo run -q -p ryo-v2-cli -- bench "$@"
    ;;
  *)
    echo "usage: $0 [check|oracle|all|bench ARGS...]" >&2
    exit 2
    ;;
esac
echo "verify: $mode ok"
