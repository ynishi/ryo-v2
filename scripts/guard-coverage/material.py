"""Labelling material per sampled commit (read-only git).

usage: python3 -I material.py <repo> <facts.json> <outdir>
Writes <outdir>/m/NNN.txt (message, numstat, truncated .rs diff: production code first,
then test-path files, then e2e fixtures) and <outdir>/full/NNN.diff (message and the whole .rs diff).
"""
import json
import os
import subprocess
import sys

repo, facts_path, outdir = sys.argv[1:4]
CAP = {"prod": 260, "test": 70, "fixture": 30}


def git(*args):
    return subprocess.run(["git", "-C", repo, *args], check=True, capture_output=True,
                          text=True, errors="replace").stdout


def is_test_path(p):
    parts = p.split("/")
    name = parts[-1]
    if any(seg in ("tests", "benches", "test") for seg in parts[:-1]):
        return True
    if name in ("tests.rs", "test.rs"):
        return True
    return name.endswith("_test.rs") or name.endswith("_tests.rs") or name.startswith("test_")


def kind(p):
    if p.startswith("examples/e2e/"):
        return "fixture"
    return "test" if is_test_path(p) else "prod"


os.makedirs(os.path.join(outdir, "m"), exist_ok=True)
os.makedirs(os.path.join(outdir, "full"), exist_ok=True)
rows = json.load(open(facts_path))
sizes = []
for n, r in enumerate(rows, 1):
    h = r["hash"]
    msg = git("show", "-s", "--format=%B", h).rstrip()
    numstat = git("show", "--format=", "--numstat", "--no-renames", h).rstrip()
    rs_paths = [l.split("\t", 2)[2] for l in numstat.splitlines()
                if l.strip() and l.split("\t", 2)[2].endswith(".rs")]
    groups = {"prod": [], "test": [], "fixture": []}
    for p in rs_paths:
        groups[kind(p)].append(p)
    full_parts, trunc_parts = [], []
    for g in ("prod", "test", "fixture"):
        if not groups[g]:
            continue
        d = git("show", "--format=", "--no-renames", h, "--", *groups[g])
        lines = d.splitlines()
        full_parts.append(f"===== {g} .rs files ({len(groups[g])}) =====\n{d}")
        shown = lines[:CAP[g]]
        note = ""
        if len(lines) > CAP[g]:
            note = f"\n[... {g} diff truncated: {CAP[g]} of {len(lines)} lines shown; full diff in full/{n:03d}.diff]"
        trunc_parts.append(f"===== {g} .rs files ({len(groups[g])}) =====\n" + "\n".join(shown) + note)
    header = f"# commit {n:03d}  id {r['id']}  date {r['date']}\n"
    body = (header + "\n--- message ---\n" + msg + "\n\n--- files changed (added, deleted, path) ---\n"
            + numstat + "\n\n--- .rs diff ---\n")
    open(os.path.join(outdir, "m", f"{n:03d}.txt"), "w").write(body + "\n\n".join(trunc_parts) + "\n")
    open(os.path.join(outdir, "full", f"{n:03d}.diff"), "w").write(body + "\n\n".join(full_parts) + "\n")
    sizes.append(len(body) + sum(len(t) for t in trunc_parts))
print("material files:", len(rows), "total chars:", sum(sizes), "max:", max(sizes))
