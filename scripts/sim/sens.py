"""Sensitivity of scenario S3 of fork_sim.py (item scope, lines the AI writes, first-pass rate).

usage: python3 -I scripts/sim/sens.py (imports fork_sim from its own directory)
"""
import random, copy
import fork_sim as fs
random.seed(7)
base = fs.SCENARIOS["S3 cross-cutting, 5% files, ~10k lines"]
cm0 = fs.COMMON
cases = [
    ("as above", dict(), dict()),
    ("item scope (f_big 0.30->0.15, f_delta 0.015->0.005)", dict(f_big=0.15), dict(f_delta=0.005)),
    ("+ AI writes 300 lines (more done by refactor calls)", dict(f_big=0.15, ai_lines=300), dict(f_delta=0.005)),
    ("+ first-pass rate 0.30", dict(f_big=0.15, ai_lines=300, p0=0.30), dict(f_delta=0.005)),
]
modes = [("M1 stacked, 1 branch","M1",1,0.7),("M2 + 4 forks a=0.5","M2",4,0.5),("M2 + 4 forks a=0.7","M2",4,0.7),("M2 + 8 forks a=0.5","M2",8,0.5),("M2 + 8 forks a=0.7","M2",8,0.7)]
print("| S3 variant | mode | P(10 passed <= 60 s) | median time to 10 | P(first <= 60 s) | median first | mean passed by 60 s |")
print("|---|---|---|---|---|---|---|")
for label, sc_over, cm_over in cases:
    sc = dict(base, **sc_over); cm = dict(cm0, **cm_over)
    for ml, m, c, a in modes:
        rows = [fs.trial(sc, m, c, a, cm) for _ in range(3000)]
        s = fs.summarise(rows)
        print(f"| {label} | {ml} | {s['p_goal_60']:.2f} | {s['med_goal']:.0f} s | {s['p_first_60']:.2f} | {s['med_first']:.0f} s | {s['mean_passed_60']:.1f} |")
