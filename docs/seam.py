import os
exec(open(os.path.join(os.path.dirname(__file__), 'cancel.py')).read().split('# ---- experiment ----')[0])
from collections import defaultdict
import math
EXTENT, MID = 4096, 2048
QUADS = [(0,0,MID,MID),(MID,0,EXTENT,MID),(0,MID,MID,EXTENT),(MID,MID,EXTENT,EXTENT)]
def sgn(a): return "+" if a>0 else ("-" if a<0 else "0")

STATS = defaultdict(int)

def merge_by_seam_sweep(rings, seams_x, seams_y):
    """Cancel only along the named seam lines, by a signed 1-D sweep.

    Everything not lying on a seam passes straight through: it has nothing it
    could possibly match.
    """
    survivors = []
    on_x = defaultdict(list)     # x -> [(lo, hi, weight)]
    on_y = defaultdict(list)

    for ring in rings:
        n = len(ring)
        for i in range(n):
            a, b = ring[i], ring[(i+1) % n]
            if a == b:
                continue
            STATS["edges"] += 1
            if a[0] == b[0] and a[0] in seams_x:
                lo, hi = min(a[1], b[1]), max(a[1], b[1])
                on_x[a[0]].append((lo, hi, 1 if b[1] > a[1] else -1))
                STATS["on_seam"] += 1
            elif a[1] == b[1] and a[1] in seams_y:
                lo, hi = min(a[0], b[0]), max(a[0], b[0])
                on_y[a[1]].append((lo, hi, 1 if b[0] > a[0] else -1))
                STATS["on_seam"] += 1
            else:
                survivors.append((a, b))

    # Signed 1-D sweep per seam. Splitting at every endpoint is inherent, so
    # partially overlapping edges need no special handling.
    for x, spans in on_x.items():
        for lo, hi, net in sweep(spans):
            for _ in range(abs(net)):
                survivors.append(((x, lo), (x, hi)) if net > 0 else ((x, hi), (x, lo)))
    for y, spans in on_y.items():
        for lo, hi, net in sweep(spans):
            for _ in range(abs(net)):
                survivors.append(((lo, y), (hi, y)) if net > 0 else ((hi, y), (lo, y)))

    return rejoin(survivors)

def sweep(spans):
    """Elementary intervals with a non-zero net winding."""
    cuts = sorted({p for lo, hi, _ in spans for p in (lo, hi)})
    out = []
    for lo, hi in zip(cuts, cuts[1:]):
        net = sum(w for s_lo, s_hi, w in spans if s_lo <= lo and hi <= s_hi)
        if net:
            out.append((lo, hi, net))
    return out

def rejoin(edges):
    outgoing = defaultdict(list)
    for a, b in edges:
        outgoing[a].append(b)
    rings = []
    for start in list(outgoing):
        while outgoing[start]:
            ring, node = [start], start
            while outgoing[node]:
                node = outgoing[node].pop()
                ring.append(node)
                if node == start:
                    break
            if len(ring) > 3:
                rings.append(ring[:-1])
            else:
                break
    return rings

def run(label, rings):
    truth = sum(shoelace(w) for w in
                (clip_ring(r,0,0,EXTENT,EXTENT) for r in rings) if len(w) > 2)
    pieces = [p for q in QUADS for r in rings if len(p := clip_ring(r, *q)) > 2]
    merged = [m for m in merge_by_seam_sweep(pieces, {MID}, {MID}) if abs(shoelace(m)) > 0]
    areas = sorted((shoelace(m) for m in merged), key=lambda a: -a)
    ok = abs(sum(areas) - truth) < 1
    print(f"  {label:44} -> {len(merged)} ring(s) signs {''.join(sgn(a) for a in areas):<4} "
          f"{'OK' if ok else '*** MISMATCH ***'}")

step  = [(0,0),(3000,0),(3000,1000),(MID,1000),(MID,EXTENT),(0,EXTENT)]
step2 = [(EXTENT,0),(EXTENT,EXTENT),(MID,EXTENT),(MID,1000),(1000,1000),(1000,0)]
zig   = [(0,0),(MID,0),(MID,800),(2500,800),(2500,1600),(MID,1600),(MID,EXTENT),(0,EXTENT)]
notch = [(0,0),(EXTENT,0),(EXTENT,EXTENT),(0,EXTENT)]
bite  = [(MID,1500),(MID,2500),(3500,2500),(3500,1500)]
holeC = [(MID-400,MID-400),(MID-400,MID+400),(MID+400,MID+400),(MID+400,MID-400)]
big   = [(-500,-500),(EXTENT+500,-500),(EXTENT+500,EXTENT+500),(-500,EXTENT+500)]
blob  = [(MID + 1700*math.cos(t)*(1+0.35*math.sin(5*t)),
          MID + 1700*math.sin(t)*(1+0.35*math.cos(4*t)))
         for t in [i*2*math.pi/400 for i in range(400)]]

print("Seam sweep (no separate collinear-splitting pass):")
for label, rings in [
    ("step (real edge on seam)",   [step]),
    ("mirrored step",              [step2]),
    ("leaves and rejoins seam",    [zig]),
    ("hole flush on seam",         [notch, bite]),
    ("hole across seam cross",     [notch, holeC]),
    ("covers whole tile",          [big]),
    ("lumpy 400-vertex blob",      [blob]),
]:
    STATS.clear(); run(label, rings)
    print(f"       edges {STATS['edges']:>5}   on a seam {STATS['on_seam']:>4}  "
          f"({100*STATS['on_seam']/max(STATS['edges'],1):.0f}%)")
