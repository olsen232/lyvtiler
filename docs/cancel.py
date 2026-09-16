"""Can 4 clipped quadrant pieces be rejoined without a boolean library?

Proposed algorithm: collect every directed edge of every piece, cancel
coincident opposite pairs, rejoin survivors into rings.
"""
import math
from collections import defaultdict

# ---- Sutherland-Hodgman clip, integer output ----
def clip_ring(ring, xmin, ymin, xmax, ymax):
    def inside(p, edge):
        x, y = p
        return [x >= xmin, x <= xmax, y >= ymin, y <= ymax][edge]
    def intersect(s, e, edge):
        (x1, y1), (x2, y2) = s, e
        if edge == 0: x = xmin
        elif edge == 1: x = xmax
        else: x = None
        if x is not None:
            t = (x - x1) / (x2 - x1)
            return (x, y1 + t * (y2 - y1))
        y = ymin if edge == 2 else ymax
        t = (y - y1) / (y2 - y1)
        return (x1 + t * (x2 - x1), y)

    out = list(ring)
    for edge in range(4):
        if not out: break
        inp, out = out, []
        S = inp[-1]
        for E in inp:
            if inside(S, edge):
                out.append(S)
                if not inside(E, edge):
                    out.append(intersect(S, E, edge))
            elif inside(E, edge):
                out.append(intersect(S, E, edge))
            S = E
    return [(round(x), round(y)) for x, y in out]

def shoelace(ring):
    n = len(ring)
    return sum(ring[i][0]*ring[(i+1) % n][1] - ring[(i+1) % n][0]*ring[i][1]
               for i in range(n)) / 2

# ---- the proposed merge ----
def merge_by_cancellation(pieces):
    """pieces: list of rings (closed implicitly). Returns merged rings."""
    edges = defaultdict(int)
    for ring in pieces:
        n = len(ring)
        for i in range(n):
            a, b = ring[i], ring[(i + 1) % n]
            if a == b:
                continue
            # Count directed edges; an edge and its reverse cancel.
            key = (a, b) if a < b else (b, a)
            edges[key] += 1 if (a, b) == key else -1

    survivors = []
    for (a, b), net in edges.items():
        if net > 0:
            survivors += [(a, b)] * net
        elif net < 0:
            survivors += [(b, a)] * (-net)

    # Rejoin survivors into rings by following endpoints.
    outgoing = defaultdict(list)
    for a, b in survivors:
        outgoing[a].append(b)

    rings, used = [], set()
    for start in list(outgoing):
        while outgoing[start]:
            ring, node = [start], start
            while outgoing[node]:
                nxt = outgoing[node].pop()
                ring.append(nxt)
                node = nxt
                if node == start:
                    break
            if len(ring) > 3:
                rings.append(ring[:-1])
            else:
                break
    return rings

# ---- experiment ----
EXTENT = 4096
MID = EXTENT // 2

# A lumpy blob that crosses both internal boundaries, spanning the parent tile.
blob = [(MID + 1700 * math.cos(t) * (1 + 0.35 * math.sin(5 * t)),
         MID + 1700 * math.sin(t) * (1 + 0.35 * math.cos(4 * t)))
        for t in [i * 2 * math.pi / 400 for i in range(400)]]

whole = clip_ring(blob, 0, 0, EXTENT, EXTENT)
print(f"feature clipped to the parent tile: {len(whole)} vertices, "
      f"area {shoelace(whole):,.0f}")

quadrants = [(0, 0, MID, MID), (MID, 0, EXTENT, MID),
             (0, MID, MID, EXTENT), (MID, MID, EXTENT, EXTENT)]
pieces = [clip_ring(blob, *q) for q in quadrants]
pieces = [p for p in pieces if len(p) > 2]
print(f"split into {len(pieces)} quadrant pieces: "
      f"{[len(p) for p in pieces]} vertices, areas {[f'{shoelace(p):,.0f}' for p in pieces]}")
print(f"  sum of piece areas: {sum(shoelace(p) for p in pieces):,.0f}")

merged = merge_by_cancellation(pieces)
print(f"\nafter edge cancellation: {len(merged)} ring(s), "
      f"{[len(r) for r in merged]} vertices, areas {[f'{shoelace(r):,.0f}' for r in merged]}")
print(f"  total merged area: {sum(shoelace(r) for r in merged):,.0f}")
print(f"  matches whole-feature area: "
      f"{abs(sum(shoelace(r) for r in merged) - shoelace(whole)) < 1}")
