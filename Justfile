
# `check` is the definition of green. CI runs it and nothing else, so a gate
# added here is picked up there without a second edit.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Everything a change owes before it is handed over.
check: rust-fmt-check rust-clippy rust-test deny

rust-fmt-check:
    cargo fmt --all -- --check

# Warnings fail the build through Cargo's `build.warnings` rather than
# `-- -D warnings`, which changes the compiler flags and so invalidates the
# build cache shared with `cargo build` and `cargo test`.
rust-clippy:
    CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets

# `--locked`: a Cargo.lock that does not match the manifests fails the gate
# instead of being rewritten.
rust-test:
    cargo test --workspace --locked

deny:
    cargo deny check

# Compares ryo-v2-ra with corpus/*/expected. Never runs cargo check.
oracle:
    cargo test -p ryo-v2-oracle --test oracle -- --nocapture

# The oracle with the proc-macro server off (prints the table; floors are not checked).
oracle-off:
    RYO_V2_ORACLE_PROC_MACRO=off cargo test -p ryo-v2-oracle --test oracle -- --nocapture

# Rewrites corpus/<ws>/expected/*.check.json from `cargo check --workspace --all-targets`: the only binary that runs cargo check on the corpus (`oracle-refresh-all` calls it per case).
oracle-refresh ws="base/extract-trait-multicrate":
    cargo run -p ryo-v2-oracle --bin oracle-refresh -- {{ws}}

# Runs oracle-refresh on every case of every corpus/base workspace (or those matching <only>) and prints, per workspace, PASS (no refusal, expected unchanged) or FAIL with the refused cases and the diff of the expected files. Exits 1 on any refusal or change; the rewritten expected files stay in place.
oracle-refresh-all only="*":
    #!/usr/bin/env bash
    set -uo pipefail
    snap="$(mktemp -d)"
    trap 'rm -r "$snap"' EXIT
    cargo build -q -p ryo-v2-oracle --bin oracle-refresh
    bad=0
    for cases in corpus/base/{{only}}/cases; do
        dir="${cases%/cases}"
        ws="${dir#corpus/}"
        mkdir -p "$snap/$ws"
        [ -d "$dir/expected" ] && cp -r "$dir/expected" "$snap/$ws/expected"
        n=0
        refused=()
        for f in "$cases"/*.toml; do
            id="$(basename "$f" .toml)"
            n=$((n + 1))
            if ! out="$(cargo run -q -p ryo-v2-oracle --bin oracle-refresh -- "$ws" --case "$id" 2>&1)"; then
                refused+=("$id")
                printf '%s\n' "$out"
            fi
        done
        if d="$(diff -ruN "$snap/$ws/expected" "$dir/expected")"; then changed=0; else changed=1; fi
        if [ "${#refused[@]}" -eq 0 ] && [ "$changed" -eq 0 ]; then
            echo "oracle-refresh $ws: PASS ($n cases, expected unchanged)"
        else
            bad=1
            echo "oracle-refresh $ws: FAIL ($n cases, refused: ${refused[*]:-none}, expected changed: $changed)"
            [ "$changed" -eq 1 ] && printf '%s\n' "$d"
        fi
    done
    exit "$bad"

# History precision, step 1 (git only): writes the (parent, child) pairs of <repo> <range> under <out>/<name>. <out> must have no `target` component.
history-export repo range name out max="20":
    cargo run -q -p ryo-v2-oracle --bin history-cases -- export {{repo}} {{range}} --name {{name}} --out {{out}} --max {{max}}

# History precision, step 2: cargo check --all-targets on each child of <out>/<name>; writes expected.check.json per pair.
history-expect dir:
    cargo run -q -p ryo-v2-oracle --bin history-cases -- expect {{dir}}

# History precision, step 3: the verifier on every pair under <out>; prints the false-positive rate per set.
oracle-history out:
    RYO_V2_HISTORY_DIR={{out}} cargo test -p ryo-v2-oracle --test history -- --nocapture

# Writes breakage cases and overlays for corpus/<ws> (deterministic; commit the output).
breakage-gen ws kinds="all" seeds="3":
    cargo run -p ryo-v2-oracle --bin breakage -- gen {{ws}} --kinds {{kinds}} --seeds {{seeds}}

# Writes planted cases for <ws> (corpus-relative, e.g. base/breakage-lab or search/base/search-lab) under corpus/search (deterministic; commit the output).
search-recall-gen ws seeds="3":
    cargo run -q -p ryo-v2-oracle --bin search-recall -- gen {{ws}} --seeds {{seeds}}

# H6: recall@20 of the search on the planted cases under <dir>; prints the table (SEARCH_LEDGER.md has the numbers).
search-recall dir="corpus/search":
    cargo run -q -p ryo-v2-oracle --bin search-recall -- run {{dir}}

# Sets the group of breakage cases from their expected files.
breakage-regroup ws:
    cargo run -p ryo-v2-oracle --bin breakage -- regroup {{ws}}

# One worktree per issue, cut from a freshly fetched origin/main.
worktree-new type slug:
    git fetch origin
    git worktree add .worktrees/{{slug}} -b {{type}}/{{slug}} origin/main
    cd .worktrees/{{slug}} && just branch-check

# Refuses main, and a branch that does not contain origin/main.
branch-check:
    #!/usr/bin/env bash
    set -euo pipefail
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" = "main" ]; then
        echo "branch-check: on main; cut a branch with 'just worktree-new'" >&2
        exit 1
    fi
    if git rev-parse --verify --quiet origin/main >/dev/null; then
        git merge-base --is-ancestor origin/main HEAD || {
            echo "branch-check: $branch does not contain origin/main; rebase or recut" >&2
            exit 1
        }
    fi
    echo "branch-check: $branch ok"

# Commit messages on this branch carry no AI attribution (CONTRIBUTING.md).
commit-msg-check:
    #!/usr/bin/env bash
    set -euo pipefail
    range="origin/main..HEAD"
    git rev-parse --verify --quiet origin/main >/dev/null || range="HEAD"
    if git log --format=%B "$range" | grep -inE '^(co-authored-by|signed-off-by):|generated with'; then
        echo "commit-msg-check: attribution lines found in $range" >&2
        exit 1
    fi
    echo "commit-msg-check: $range ok"

# The gate before a hand-over: run it after the last commit, over the tree
# that is actually handed over.
pre-push: branch-check commit-msg-check check

