"""Agreement and shares for the guard-coverage labels.

Guard coverage: for a sample of commits of a verification repository used as a sample, which
guard layer (G0 equivalence .. G5 intent) each change needs, labelled by two labellers blind to
each other. The pipeline is facts.py -> material.py -> labelling -> analysis.py. The labels and
the material quote the sample's commits and are kept outside this repository.

usage: python3 -I analysis.py <gc dir> [resolved.tsv]
Reads <gc>/labels/A/*.tsv, <gc>/labels/B/*.tsv and <gc>/data/facts.json.
Without resolved.tsv it prints the agreement and the disagreements; with it, also the shares.
"""
import collections
import glob
import json
import math
import os
import random
import sys

CATS = ["NC", "G0", "G1", "G2", "G3", "G4", "G5"]
RANK = {c: i for i, c in enumerate(CATS)}


def parse(path_glob):
    rows = {}
    for path in sorted(glob.glob(path_glob)):
        for line in open(path, encoding="utf-8"):
            line = line.rstrip("\n")
            if not line.strip():
                continue
            parts = line.split("\t")
            if len(parts) < 6:
                raise SystemExit(f"{path}: bad line: {line!r}")
            n, cid, layers, highest, distinct = (p.strip() for p in parts[:5])
            reason = "\t".join(parts[5:]).strip()
            n = int(n)
            ls = [x.strip() for x in layers.split(",") if x.strip()]
            for x in ls + [highest]:
                if x not in RANK:
                    raise SystemExit(f"{path}: unknown layer {x!r} in {line!r}")
            if ls == ["NC"]:
                exp = "NC"
            else:
                if "NC" in ls:
                    raise SystemExit(f"{path}: NC mixed with layers: {line!r}")
                exp = max(ls, key=RANK.get)
            if exp != highest:
                raise SystemExit(f"{path}: highest {highest} is not the max of {ls}: {line!r}")
            if n in rows:
                raise SystemExit(f"{path}: duplicate {n}")
            rows[n] = {"id": cid, "layers": set(ls), "highest": highest,
                       "distinct": distinct, "reason": reason}
    return rows


def binom_cdf(k, n, p):
    return sum(math.comb(n, i) * p ** i * (1 - p) ** (n - i) for i in range(0, k + 1))


def clopper_pearson(k, n, alpha=0.05):
    # lower: the p with P(X >= k) = alpha/2 (P(X >= k) increases in p);
    # upper: the p with P(X <= k) = alpha/2 (P(X <= k) decreases in p).
    lower = 0.0
    if k > 0:
        lo, hi = 0.0, 1.0
        for _ in range(200):
            mid = (lo + hi) / 2
            if 1 - binom_cdf(k - 1, n, mid) < alpha / 2:
                lo = mid
            else:
                hi = mid
        lower = hi
    upper = 1.0
    if k < n:
        lo, hi = 0.0, 1.0
        for _ in range(200):
            mid = (lo + hi) / 2
            if binom_cdf(k, n, mid) > alpha / 2:
                lo = mid
            else:
                hi = mid
        upper = lo
    return lower, upper


def kappa(a, b, cats):
    n = len(a)
    po = sum(x == y for x, y in zip(a, b)) / n
    ca, cb = collections.Counter(a), collections.Counter(b)
    pe = sum(ca[c] / n * cb[c] / n for c in cats)
    return po, pe, (po - pe) / (1 - pe) if pe < 1 else float("nan")


def kappa_boot(a, b, cats, reps=5000, seed=20261101):
    r = random.Random(seed)
    n = len(a)
    ks = []
    for _ in range(reps):
        idx = [r.randrange(n) for _ in range(n)]
        _, _, k = kappa([a[i] for i in idx], [b[i] for i in idx], cats)
        if not math.isnan(k):
            ks.append(k)
    ks.sort()
    return ks[int(0.025 * len(ks))], ks[int(0.975 * len(ks)) - 1]


def pct(x):
    return f"{100 * x:.1f}"


def main():
    gc = sys.argv[1]
    facts = {i + 1: r for i, r in enumerate(json.load(open(os.path.join(gc, "data/facts.json"))))}
    A = parse(os.path.join(gc, "labels/A/*.tsv"))
    B = parse(os.path.join(gc, "labels/B/*.tsv"))
    for name, L in (("A", A), ("B", B)):
        missing = sorted(set(facts) - set(L))
        if missing:
            raise SystemExit(f"labeller {name}: missing {missing}")
        for n, r in L.items():
            if not facts[n]["id"].startswith(r["id"][:7]) and not r["id"].startswith(facts[n]["id"][:7]):
                raise SystemExit(f"labeller {name}: id mismatch at {n}: {r['id']} vs {facts[n]['id']}")
    ns = sorted(facts)
    ha = [A[n]["highest"] for n in ns]
    hb = [B[n]["highest"] for n in ns]
    po, pe, k = kappa(ha, hb, CATS)
    klo, khi = kappa_boot(ha, hb, CATS)
    print(f"highest layer, all {len(ns)}: agree {sum(x == y for x, y in zip(ha, hb))}/{len(ns)} = {pct(po)}%, p_e {pct(pe)}%, kappa {k:.2f} (bootstrap 95% {klo:.2f}..{khi:.2f})")
    for name, f in (("code vs NC", lambda h: "NC" if h == "NC" else "code"),
                    ("in loop (<=G3) vs not, NC kept", lambda h: h if h == "NC" else ("in" if RANK[h] <= RANK["G3"] else "out"))):
        xa, xb = [f(h) for h in ha], [f(h) for h in hb]
        cats = sorted(set(xa) | set(xb))
        po2, pe2, k2 = kappa(xa, xb, cats)
        lo2, hi2 = kappa_boot(xa, xb, cats)
        print(f"{name}: agree {pct(po2)}%, kappa {k2:.2f} (bootstrap 95% {lo2:.2f}..{hi2:.2f})")
    # per-layer presence agreement (both say code)
    both_code = [n for n in ns if A[n]["highest"] != "NC" and B[n]["highest"] != "NC"]
    for L in CATS[1:]:
        xa = [L in A[n]["layers"] for n in both_code]
        xb = [L in B[n]["layers"] for n in both_code]
        po3, pe3, k3 = kappa(xa, xb, [True, False])
        print(f"presence {L} (both code, n={len(both_code)}): A {sum(xa)}, B {sum(xb)}, agree {pct(po3)}%, kappa {k3:.2f}")
    print("confusion (rows A, cols B):")
    print("      " + " ".join(f"{c:>4}" for c in CATS))
    for ca in CATS:
        print(f"{ca:>4}  " + " ".join(f"{sum(1 for x, y in zip(ha, hb) if x == ca and y == cb):>4}" for cb in CATS))
    print("marginals A:", dict(collections.Counter(ha)), " B:", dict(collections.Counter(hb)))
    dis = [n for n in ns if A[n]["highest"] != B[n]["highest"]]
    print(f"disagreements on highest: {len(dis)}: {' '.join(f'{n:03d}' for n in dis)}")
    if len(sys.argv) < 3:
        return
    R = parse(sys.argv[2])
    if sorted(R) != ns:
        raise SystemExit("resolved: not all commits")
    code = [n for n in ns if R[n]["highest"] != "NC"]
    nc = [n for n in ns if R[n]["highest"] == "NC"]
    N = len(code)
    print(f"\nresolved: code {N}, no code {len(nc)} ({' '.join(f'{n:03d}' for n in nc)})")
    lines = {n: facts[n]["rs_added"] + facts[n]["rs_deleted"] for n in ns}
    tot_lines = sum(lines[n] for n in code)
    print("highest layer shares of code commits (Clopper-Pearson 95%), and line share:")
    for L in CATS[1:]:
        kk = sum(1 for n in code if R[n]["highest"] == L)
        lo, hi = clopper_pearson(kk, N)
        ls = sum(lines[n] for n in code if R[n]["highest"] == L) / tot_lines
        print(f"  {L}: {kk}/{N} = {pct(kk / N)}% ({pct(lo)}..{pct(hi)}); lines {pct(ls)}%")

    def share(pred, label):
        kk = sum(1 for n in code if pred(n))
        lo, hi = clopper_pearson(kk, N)
        print(f"{label}: {kk}/{N} = {pct(kk / N)}% ({pct(lo)}..{pct(hi)})")
        return kk

    share(lambda n: RANK[R[n]["highest"]] <= RANK["G3"], "in loop (highest <= G3)")
    share(lambda n: RANK[R[n]["highest"]] <= RANK["G1"], "compiler only (highest <= G1)")
    share(lambda n: R[n]["layers"] == {"G1"}, "G1 alone (layers == {G1})")
    for L in CATS[1:]:
        share(lambda n, L=L: L in R[n]["layers"], f"needs {L} at all")
    share(lambda n: "G1" in R[n]["layers"] and RANK[R[n]["highest"]] >= RANK["G4"], "G1 part of a G4/G5 commit")
    share(lambda n: "G2" in R[n]["layers"] and RANK[R[n]["highest"]] >= RANK["G4"], "G2 part of a G4/G5 commit")
    # weighted
    inl = lambda n: RANK[R[n]["highest"]] <= RANK["G3"]
    w = sum(lines[n] for n in code if inl(n)) / tot_lines
    r = random.Random(20261101)
    bs = []
    for _ in range(10000):
        idx = [code[r.randrange(N)] for _ in range(N)]
        t = sum(lines[i] for i in idx)
        bs.append(sum(lines[i] for i in idx if inl(i)) / t)
    bs.sort()
    print(f"in loop weighted by .rs lines changed: {pct(w)}% of {tot_lines} lines (bootstrap 95% {pct(bs[250])}..{pct(bs[9749])})")
    w1 = sum(lines[n] for n in code if R[n]["layers"] == {"G1"}) / tot_lines
    print(f"G1 alone weighted by lines: {pct(w1)}%")
    # sizes by highest
    for L in CATS[1:]:
        s = sorted(lines[n] for n in code if R[n]["highest"] == L)
        if s:
            print(f"  lines {L}: n {len(s)}, median {s[len(s) // 2]}, total {sum(s)}")
    # distinct-type cut
    g12 = [n for n in code if R[n]["highest"] in ("G1", "G2")]
    dy = [n for n in g12 if R[n]["distinct"] == "Y"]
    print(f"distinct cut: highest G1 or G2: {len(g12)}; distinct type needed: {len(dy)} ({' '.join(f'{n:03d}' for n in dy)})")
    g1any = [n for n in code if R[n]["layers"] & {"G1", "G2"}]
    dyany = [n for n in g1any if R[n]["distinct"] == "Y"]
    print(f"distinct cut, any G1/G2 part: {len(g1any)}; distinct Y: {len(dyany)}")
    # mechanical facts against the resolved layers
    print("conventional type x highest:")
    ct = collections.defaultdict(collections.Counter)
    for n in ns:
        ct[facts[n]["cc_type"] or "(none)"][R[n]["highest"]] += 1
    for t, c in sorted(ct.items(), key=lambda kv: -sum(kv[1].values())):
        print(f"  {t:>9}: " + " ".join(f"{L} {c[L]}" for L in CATS if c[L]))
    print("test file (path) changed x highest:")
    for flag in (True, False):
        c = collections.Counter(R[n]["highest"] for n in code
                                if (facts[n]["rs_test_path_files"] > 0 or facts[n]["test_attr_in_diff"]
                                    or facts[n]["rs_fixture_files"] > 0) == flag)
        print(f"  tests touched {flag}: " + " ".join(f"{L} {c[L]}" for L in CATS if c[L]))


if __name__ == "__main__":
    main()
