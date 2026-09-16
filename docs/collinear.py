import os
exec(open(os.path.join(os.path.dirname(__file__), 'cancel.py')).read().split('# ---- experiment ----')[0])
from collections import defaultdict
EXTENT, MID = 4096, 2048
QUADS = [(0,0,MID,MID),(MID,0,EXTENT,MID),(0,MID,MID,EXTENT),(MID,MID,EXTENT,EXTENT)]
def sgn(a): return "+" if a>0 else ("-" if a<0 else "0")

def split_collinear_overlaps(rings):
    """Split axis-parallel edges that lie on a common line at each other's
    endpoints, so partially overlapping opposite edges become exactly
    matching pairs that cancellation can annihilate."""
    # Gather the cut points present on each supporting line.
    cuts_v = defaultdict(set)   # x -> set of y
    cuts_h = defaultdict(set)   # y -> set of x
    for ring in rings:
        n = len(ring)
        for i in range(n):
            a, b = ring[i], ring[(i+1) % n]
            if a[0] == b[0]:
                cuts_v[a[0]].update((a[1], b[1]))
            if a[1] == b[1]:
                cuts_h[a[1]].update((a[0], b[0]))

    out = []
    for ring in rings:
        new = []
        n = len(ring)
        for i in range(n):
            a, b = ring[i], ring[(i+1) % n]
            new.append(a)
            if a[0] == b[0] and a[1] != b[1]:            # vertical
                lo, hi = sorted((a[1], b[1]))
                inner = sorted(y for y in cuts_v[a[0]] if lo < y < hi)
                if a[1] > b[1]:
                    inner.reverse()
                new.extend((a[0], y) for y in inner)
            elif a[1] == b[1] and a[0] != b[0]:          # horizontal
                lo, hi = sorted((a[0], b[0]))
                inner = sorted(x for x in cuts_h[a[1]] if lo < x < hi)
                if a[0] > b[0]:
                    inner.reverse()
                new.extend((x, a[1]) for x in inner)
        out.append(new)
    return out

def run(label, rings, fix):
    truth = sum(shoelace(w) for w in
                (clip_ring(r,0,0,EXTENT,EXTENT) for r in rings) if len(w) > 2)
    pieces = [p for q in QUADS for r in rings if len(p := clip_ring(r, *q)) > 2]
    if fix:
        pieces = split_collinear_overlaps(pieces)
    merged = [m for m in merge_by_cancellation(pieces) if abs(shoelace(m)) > 0]
    areas = sorted((shoelace(m) for m in merged), key=lambda a: -a)
    total = sum(areas)
    flag = "OK" if abs(total - truth) < 1 else "*** AREA MISMATCH ***"
    print(f"  {label:44} -> {len(merged)} ring(s) signs {''.join(sgn(a) for a in areas):<4} {flag}")

step  = [(0,0),(3000,0),(3000,1000),(MID,1000),(MID,EXTENT),(0,EXTENT)]
step2 = [(EXTENT,0),(EXTENT,EXTENT),(MID,EXTENT),(MID,1000),(1000,1000),(1000,0)]
zig   = [(0,0),(MID,0),(MID,800),(2500,800),(2500,1600),(MID,1600),(MID,EXTENT),(0,EXTENT)]
notch = [(0,0),(EXTENT,0),(EXTENT,EXTENT),(0,EXTENT)]
bite  = [(MID,1500),(MID,2500),(3500,2500),(3500,1500)]
holeC = [(MID-400,MID-400),(MID-400,MID+400),(MID+400,MID+400),(MID+400,MID-400)]

cases = [("step (real edge on seam)", [step]),
         ("mirrored step",            [step2]),
         ("leaves and rejoins seam",  [zig]),
         ("hole flush on seam",       [notch, bite]),
         ("hole across seam cross",   [notch, holeC])]

for fix in (False, True):
    print(f"\n{'WITH collinear splitting' if fix else 'WITHOUT collinear splitting (current)'}:")
    for label, rings in cases:
        run(label, rings, fix)
