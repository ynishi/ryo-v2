
# `check` is the definition of green. CI runs it and nothing else, so a gate
# added here is picked up there without a second edit.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Everything a change owes before it is handed over.
check: base-check rust-fmt-check rust-clippy rust-test deny

rust-fmt-check:
    cargo fmt --all -- --check

# Warnings fail the build through Cargo's `build.warnings` rather than
# `-- -D warnings`, which changes the compiler flags and so invalidates the
# build cache shared with `cargo build` and `cargo test`.
rust-clippy:
    CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked

# `--locked` (here and in `rust-clippy`, which runs first): a Cargo.lock that
# does not match the manifests fails the gate instead of being rewritten.
rust-test:
    cargo test --workspace --locked

deny:
    cargo deny check

# The vendored salsa against upstream (vendor/VENDORED, D11). First diffs each vendored crate
# against its crates.io package (the `.crate` in cargo's registry cache, else downloaded, checked
# against the sha256 in vendor/VENDORED) and fails on a hunk without the `ryo-v2 fork:` mark;
# then runs upstream's own suite, every case, on the toolchain salsa ships for it
# (vendor/salsa/rust-toolchain.toml; installed here, a no-op when present) with the features
# rust-analyzer enables plus `fork`. Not part of `check`; `pre-push` runs it.
salsa-upstream:
    #!/usr/bin/env bash
    set -euo pipefail
    cache="${CARGO_HOME:-$HOME/.cargo}/registry/cache"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    bad=0
    while read -r _ name version sum; do
        pkg="$name-$version"
        crate=""
        for c in "$cache"/index.crates.io-*/"$pkg.crate"; do
            if [ -f "$c" ]; then crate="$c"; break; fi
        done
        if [ -z "$crate" ]; then
            crate="$work/$pkg.crate"
            curl -sSfL "https://static.crates.io/crates/$name/$pkg.crate" -o "$crate"
        fi
        if ! echo "${sum#sha256:}  $crate" | sha256sum -c --quiet - >/dev/null; then
            echo "salsa-upstream: $pkg.crate does not have the sha256 vendor/VENDORED gives" >&2
            exit 1
        fi
        tar -xzf "$crate" -C "$work"
        rc=0
        diff -rN -U0 -x .cargo-ok -x .cargo_vcs_info.json -x Cargo.lock -x target \
            "$work/$pkg" "vendor/$name" > "$work/$name.diff" || rc=$?
        if [ "$rc" -gt 1 ]; then
            echo "salsa-upstream: diff of vendor/$name failed" >&2
            exit 1
        fi
        unmarked="$(awk '
            function flush() { if (h != "" && !m) print f ": " h; h = ""; m = 0 }
            /^diff / { flush(); f = $NF; hdr = 1; next }
            hdr && /^--- / { next }
            hdr && /^\+\+\+ / { hdr = 0; next }
            /^Binary files / { flush(); print "binary: " $0; next }
            /^@@/ { flush(); h = $0; n++; next }
            /^\+.*(\/\/|#) ryo-v2 fork:/ { m = 1 }
            END { flush(); print n + 0 > "/dev/stderr" }
        ' "$work/$name.diff" 2>"$work/$name.count")"
        if [ -n "$unmarked" ]; then
            echo "salsa-upstream: vendor/$name: hunks without the 'ryo-v2 fork:' mark:" >&2
            printf '%s\n' "$unmarked" >&2
            bad=1
        else
            echo "salsa-upstream: vendor/$name against $pkg.crate: $(cat "$work/$name.count") hunks, all marked"
        fi
    done < <(grep -E '^ +crate ' vendor/VENDORED)
    [ "$bad" -eq 0 ] || exit 1
    rustup toolchain install 1.98 --profile minimal --no-self-update
    cd vendor/salsa
    # `persistence` is added to rust-analyzer's set so that upstream's compile-fail and
    # compile-pass cases (gated on inventory and persistence) run too; `manual_registration`
    # needs `not(inventory)` and is left to upstream.
    env -u RUSTUP_TOOLCHAIN cargo test --locked --features fork,rayon,salsa_unstable,macros,inventory,triomphe,persistence

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

# Refuses main, and a branch that does not contain its base (`base-check`).
branch-check: base-check
    #!/usr/bin/env bash
    set -euo pipefail
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" = "main" ]; then
        echo "branch-check: on main; cut a branch with 'just worktree-new'" >&2
        exit 1
    fi
    echo "branch-check: $branch ok"

# Refuses a branch that does not contain `origin/main` (or the local `main` where there is no
# `origin/main`): such a branch was cut from an older main and may collide with the current one
# only after the merge. Passes on main. Says in one line that it skipped and passes outside a git
# checkout of this tree, in a shallow clone, and where neither ref exists.
base-check:
    #!/usr/bin/env bash
    set -euo pipefail
    here="$(cd "{{justfile_directory()}}" && pwd -P)"
    top="$(git -C "$here" rev-parse --show-toplevel 2>/dev/null || true)"
    if [ -z "$top" ] || [ "$(cd "$top" && pwd -P)" != "$here" ]; then
        echo "base-check: skipped (not a git checkout)"
        exit 0
    fi
    cd "$here"
    if [ "$(git rev-parse --is-shallow-repository)" = "true" ]; then
        echo "base-check: skipped (shallow clone)"
        exit 0
    fi
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" = "main" ]; then
        echo "base-check: on main"
        exit 0
    fi
    if git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then
        base="origin/main"
    elif git rev-parse --verify --quiet refs/heads/main >/dev/null; then
        base="main"
    else
        echo "base-check: skipped (no main or origin/main)"
        exit 0
    fi
    if ! git merge-base --is-ancestor "$base" HEAD; then
        echo "base-check: $branch does not contain $base; merge $base into it and check again" >&2
        exit 1
    fi
    echo "base-check: $branch contains $base"

# Registers the ledger merge driver (`.gitattributes`, crates/ryo-v2-ledger-merge) in this
# checkout's git config, which all its worktrees share: builds `ledger-merge`, copies it into the
# git directory and points `merge.ledger.driver` at the copy. Where the copy is missing, or it
# cannot merge (exit 2, the file untouched), the driver falls back to `git merge-file`, the line
# merge, with its conflict markers: a failed run never leaves ours without markers. Re-run it after
# the driver's code changes; the copy is not rebuilt by itself.
ledger-merge-install:
    #!/usr/bin/env bash
    set -euo pipefail
    built="$(cargo build --release --locked -q -p ryo-v2-ledger-merge --bin ledger-merge --message-format=json \
        | grep -o '"executable":"[^"]*"' | tail -n 1 | cut -d '"' -f 4)"
    [ -x "$built" ] || { echo "ledger-merge-install: no binary built" >&2; exit 1; }
    dest="$(cd "$(git rev-parse --git-common-dir)" && pwd -P)/ledger-merge"
    install -m 755 "$built" "$dest"
    git config merge.ledger.name "ledger rows and sections (crates/ryo-v2-ledger-merge)"
    git config merge.ledger.driver "if [ -x '$dest' ]; then '$dest' %O %A %B %L %P; s=\$?; [ \$s -le 1 ] && exit \$s; fi; git merge-file --marker-size=%L %A %O %B"
    echo "ledger-merge-install: merge.ledger.driver set"

# Runs the ledger merge driver on three files as git would, on a copy of <ours>; prints the exit
# status and the conflict marker lines, and the diff against <expected> when one is given.
ledger-merge-try base ours theirs expected="":
    #!/usr/bin/env bash
    set -uo pipefail
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    cp "{{ours}}" "$out"
    cargo run -q --locked -p ryo-v2-ledger-merge --bin ledger-merge -- "{{base}}" "$out" "{{theirs}}" 7 "{{ours}}"
    code=$?
    markers=$'^((<{7,}|>{7,}|\\|{7,})( |\r?$)|={7,}\r?$)'
    echo "ledger-merge-try: exit $code, $(grep -cE "$markers" "$out") marker lines"
    grep -nE "$markers" "$out" || true
    if [ -n "{{expected}}" ]; then
        if d="$(diff "{{expected}}" "$out")"; then
            echo "ledger-merge-try: equal to {{expected}}"
        else
            echo "ledger-merge-try: differs from {{expected}}"
            printf '%s\n' "$d"
        fi
    fi

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
# that is actually handed over. `salsa-upstream` builds the vendored salsa on its own toolchain.
pre-push: branch-check commit-msg-check check salsa-upstream

