import math, os
exec(open(os.path.join(os.path.dirname(__file__), 'cancel.py')).read().split('# ---- experiment ----')[0])

EXTENT, MID = 4096, 2048
QUADS = [(0,0,MID,MID),(MID,0,EXTENT,MID),(0,MID,MID,EXTENT),(MID,MID,EXTENT,EXTENT)]

def check(label, rings, expect_pieces=None):
    """rings: list of input rings (exterior first, then holes wound opposite)."""
    whole = [clip_ring(r, 0, 0, EXTENT, EXTENT) for r in rings]
    whole = [w for w in whole if len(w) > 2]
    whole_area = sum(shoelace(w) for w in whole)

    pieces = []
    for q in QUADS:
        for r in rings:
            p = clip_ring(r, *q)
            if len(p) > 2:
                pieces.append(p)

    merged = merge_by_cancellation(pieces)
    merged_area = sum(shoelace(m) for m in merged)
    ok = abs(merged_area - whole_area) < 1
    print(f"{label:38} {len(pieces):>3} pieces -> {len(merged)} ring(s)  "
          f"area {merged_area:>12,.0f} vs {whole_area:>12,.0f}  {'OK' if ok else 'MISMATCH'}")

# 1. A polygon that completely covers the parent tile: every quadrant piece is
#    a bare square made only of artificial edges. This is the case I expected
#    to lose the fill entirely.
big = [(-9000,-9000),(9000,-9000),(9000,9000),(-9000,9000)]
check("covers whole tile (all artificial)", [big])

# 2. Covers exactly one quadrant and nothing else.
one_quad = [(MID,MID),(EXTENT+500,MID),(EXTENT+500,EXTENT+500),(MID,EXTENT+500)]
check("covers exactly one quadrant", [one_quad])

# 3. Polygon with a hole, both crossing the internal boundaries.
ext = [(300,300),(3800,300),(3800,3800),(300,3800)]
hole = [(1500,1500),(1500,2600),(2600,2600),(2600,1500)]   # opposite winding
check("polygon with a hole across seams", [ext, hole])

# 4. Two disjoint blobs, one per diagonal quadrant.
b1 = [(200,200),(1000,200),(1000,1000),(200,1000)]
b2 = [(3000,3000),(3900,3000),(3900,3900),(3000,3900)]
check("two disjoint parts", [b1, b2])

# 5. A thin bar lying exactly along the internal boundary x=MID.
bar = [(MID-60,400),(MID+60,400),(MID+60,3700),(MID-60,3700)]
check("straddles the seam symmetrically", [bar])

# 6. A ring whose edge lies exactly ON the seam (real edge == artificial edge).
on_seam = [(MID,500),(3600,500),(3600,3600),(MID,3600)]
check("real edge sits exactly on the seam", [on_seam])

# 7. Now with halving, which is what the merge actually does.
print()
def check_halved(label, rings):
    pieces = []
    for qi, q in enumerate(QUADS):
        ox, oy = (qi % 2) * MID, (qi // 2) * MID
        for r in rings:
            p = clip_ring(r, *q)
            if len(p) > 2:
                # Child-local coords, then halve into parent space.
                local = [(x - ox, y - oy) for x, y in p]
                scaled = [(round(x/2) + ox//2, round(y/2) + oy//2) for x, y in local]
                pieces.append(scaled)
    merged = merge_by_cancellation(pieces)
    print(f"{label:38} {len(pieces):>3} pieces -> {len(merged)} ring(s)  "
          f"area {sum(shoelace(m) for m in merged):>12,.0f}")

blob = [(MID + 1700*math.cos(t)*(1+0.35*math.sin(5*t)),
         MID + 1700*math.sin(t)*(1+0.35*math.cos(4*t)))
        for t in [i*2*math.pi/400 for i in range(400)]]
check_halved("lumpy blob, halved into parent", [blob])
check_halved("hole, halved into parent", [ext, hole])
