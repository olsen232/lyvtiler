import math
import os
exec(open(os.path.join(os.path.dirname(__file__), 'cancel.py')).read().split('# ---- experiment ----')[0])

EXTENT, MID = 4096, 2048
QUADS = [(0,0,MID,MID),(MID,0,EXTENT,MID),(0,MID,MID,EXTENT),(MID,MID,EXTENT,EXTENT)]

def sgn(a): return "+" if a > 0 else ("-" if a < 0 else "0")

def run(label, rings):
    """rings: exterior CCW (positive) first, then holes CW (negative)."""
    # Ground truth: each ring clipped directly to the parent tile.
    whole = [clip_ring(r, 0, 0, EXTENT, EXTENT) for r in rings]
    whole = [w for w in whole if len(w) > 2]
    truth = sum(shoelace(w) for w in whole)

    # Cut: clip every ring to every quadrant independently, closing each
    # ring on its own (NOT unioning exterior with hole inside a tile).
    pieces = []
    for q in QUADS:
        for r in rings:
            p = clip_ring(r, *q)
            if len(p) > 2:
                pieces.append(p)

    merged = merge_by_cancellation(pieces)
    areas = sorted((shoelace(m) for m in merged), key=lambda a: -a)
    total = sum(areas)
    ok = abs(total - truth) < 1
    print(f"{label:44} {len(pieces):>2} pieces -> {len(merged)} ring(s)  "
          f"signs {''.join(sgn(a) for a in areas):<4} "
          f"total {total:>11,.0f} vs {truth:>11,.0f}  {'OK' if ok else 'MISMATCH'}")
    return areas

# Exterior covering the entire parent tile and beyond.
big = [(-500,-500),(EXTENT+500,-500),(EXTENT+500,EXTENT+500),(-500,EXTENT+500)]

def hole(x0,y0,x1,y1):
    "A hole, wound clockwise so its area is negative."
    return [(x0,y0),(x0,y1),(x1,y1),(x1,y0)]

print("Exterior covers the whole parent tile; only the hole's position varies.\n")
run("hole entirely inside one quadrant",       [big, hole(300,300,900,900)])
run("hole straddling ONE seam (vertical)",     [big, hole(MID-400,600,MID+400,1400)])
run("hole straddling BOTH seams (centred)",    [big, hole(MID-400,MID-400,MID+400,MID+400)])
run("hole exactly centred on the seam cross",  [big, hole(MID-1,MID-1,MID+1,MID+1)])
run("hole edge lying exactly ON the seam",     [big, hole(MID,600,MID+800,1400)])
run("hole crossing the parent's own boundary", [big, hole(EXTENT-300,600,EXTENT+300,1400)])
run("two holes, one per seam",                 [big, hole(MID-300,500,MID+300,900),
                                                      hole(MID-300,3000,MID+300,3400)])

print("\nExterior only partly covers the tile:\n")
blob = [(200,200),(3800,200),(3800,3800),(200,3800)]
run("smaller exterior, hole on the seam cross",[blob, hole(MID-500,MID-500,MID+500,MID+500)])
