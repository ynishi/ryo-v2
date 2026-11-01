"""Mechanical facts per sampled commit, from git only (read-only commands).

usage: python3 -I facts.py <repo> <pop.txt> <sample.txt> <out.json>
"""
import json
import re
import subprocess
import sys

repo, pop_path, sample_path, out_path = sys.argv[1:5]


def git(*args):
    return subprocess.run(
        ["git", "-C", repo, *args], check=True, capture_output=True, text=True,
        errors="replace").stdout


pop = open(pop_path).read().split()
index = {h: i + 1 for i, h in enumerate(pop)}
sample = open(sample_path).read().split()

CC = re.compile(r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^)]*)\))?(?P<bang>!)?:\s")


def is_test_path(p):
    parts = p.split("/")
    name = parts[-1]
    if any(seg in ("tests", "benches", "test") for seg in parts[:-1]):
        return True
    if name in ("tests.rs", "test.rs"):
        return True
    if name.endswith("_test.rs") or name.endswith("_tests.rs") or name.startswith("test_"):
        return True
    return False


def is_fixture_path(p):
    return p.startswith("examples/e2e/")


rows = []
for h in sample:
    meta = git("show", "-s", "--format=%h%x00%ad%x00%s%x00%P", "--date=short", h)
    short, date, subject, parents = meta.rstrip("\n").split("\x00")
    numstat = git("show", "--format=", "--numstat", "--no-renames", h)
    files = []
    for line in numstat.splitlines():
        if not line.strip():
            continue
        a, d, p = line.split("\t", 2)
        binary = a == "-"
        files.append((0 if binary else int(a), 0 if binary else int(d), p))
    rs = [f for f in files if f[2].endswith(".rs")]
    rs_test = [f for f in rs if is_test_path(f[2])]
    rs_fix = [f for f in rs if is_fixture_path(f[2])]
    diff = git("show", "--format=", "--no-renames", h, "--", "*.rs")
    changed = [l for l in diff.splitlines()
               if (l.startswith("+") or l.startswith("-")) and not l.startswith(("+++", "---"))]
    test_attr = any(re.search(r"#\[(tokio::)?test\b|#\[cfg\(test\)\]|\bmod tests\b", l) for l in changed)
    m = CC.match(subject)
    rows.append({
        "id": short,
        "hash": h,
        "index": index[h],
        "date": date,
        "cc_type": m.group("type").lower() if m else None,
        "cc_bang": bool(m and m.group("bang")),
        "files": len(files),
        "lines_added": sum(f[0] for f in files),
        "lines_deleted": sum(f[1] for f in files),
        "rs_files": len(rs),
        "rs_added": sum(f[0] for f in rs),
        "rs_deleted": sum(f[1] for f in rs),
        "rs_test_path_files": len(rs_test),
        "rs_test_path_lines": sum(f[0] + f[1] for f in rs_test),
        "rs_fixture_files": len(rs_fix),
        "rs_fixture_lines": sum(f[0] + f[1] for f in rs_fix),
        "test_attr_in_diff": test_attr,
        "non_rs_files": [f[2] for f in files if not f[2].endswith(".rs")],
    })

json.dump(rows, open(out_path, "w"), indent=1)
print(len(rows), "rows")
