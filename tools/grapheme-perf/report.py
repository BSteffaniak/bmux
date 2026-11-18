"""Run from this directory: python3 report.py [candidate CSV prefix]."""
import csv
import math
import sys
from statistics import median
from pathlib import Path


def load(prefix):
    runs = []
    for i in range(1, 4):
        with Path(f'evidence/{prefix}-{i}.csv').open() as f:
            rows = list(csv.DictReader(f))
        run = {(r['case'], int(r['n']), r['mode']): (
            median(float(r[f's{j}_ns']) for j in range(1, 6)),
            int(r['allocs']), r['digest'], int(r['bytes'])) for r in rows}
        assert len(run) == len(rows) == 84
        runs.append(run)
    assert runs[0].keys() == runs[1].keys() == runs[2].keys()
    return {k: ([run[k][0] for run in runs], [run[k][1:] for run in runs]) for k in runs[0]}


old = load('old')
spreads = sorted(max(t)/min(t)-1 for t, _ in old.values())
p95 = spreads[math.ceil(.95 * len(spreads))-1]
# Round twice the observed p95 spread up to a 5-point step, minimum 10%.
# Do not let system load grant a multi-fold slowdown: cap the performance
# allowance at 20%; flag rows exceeding this noise level as inconclusive.
margin = min(.20, max(0.10, math.ceil(2 * p95 / .05) * .05))
if len(sys.argv) == 1:
    print(f'p95 spread={p95:.6f}; maximum spread={max(spreads):.6f}; margin={margin:.6f}')
else:
    new = load(sys.argv[1])
    assert new.keys() == old.keys()
    print('case,n,mode,old_ns,new_ns,time_ratio,old_doubling,new_doubling,status')
    for k, (ot, om) in old.items():
        case,n,mode = k
        nt,nm = new[k]
        o,c = median(ot), median(nt)
        status = []
        if max(ot)/min(ot) > 1+margin or max(nt)/min(nt) > 1+margin: status.append('noisy')
        if c/o > 1+margin: status.append('time')
        if not all(a == 0 for a,_,_ in om+nm): status.append('alloc')
        if len({(d,b) for _,d,b in om+nm}) != 1: status.append('work')
        parent = case, n//2, mode
        od = nd = ''
        if parent in old:
            os = o/median(old[parent][0]); ns = c/median(new[parent][0])
            od = f'{os:.4f}'; nd = f'{ns:.4f}'
            if ns > os*(1+margin): status.append('scaling')
        print(f"{case},{n},{mode},{o:.2f},{c:.2f},{c/o:.4f},{od},{nd},{'+'.join(status) or 'pass'}")
