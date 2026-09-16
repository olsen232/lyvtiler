#!/usr/bin/env python3
"""Do coincident tile-squares survive the merge?

A tile a polygon swallows whole stores the tile square as an ordinary ring
(design notes §5). That raises a question about nesting: if a *hole* also
swallows the tile, and an inner ring swallows it again, does the tile end up
holding several coincident squares of alternating winding? And if it does, does
edge cancellation and ring reassembly cope?

This script answers both by construction:

  1. What the net winding number says the answer should be, for arbitrary
     nesting depth.
  2. What the merge actually does when handed coincident opposite squares --
     which is the part that cannot be settled by argument, because ring
     reassembly from a cancelled edge multiset is where the ambiguity lives.

Run: python3 docs/coverage.py
"""

from collections import Counter, defaultdict

E = 4096


# ---------------------------------------------------------------------------
# Rings and edges
# ---------------------------------------------------------------------------

def area(ring):
    """Shoelace area. Positive = clockwise on screen = MVT exterior."""
    total = 0
    pts = ring if ring[0] == ring[-1] else list(ring) + [ring[0]]
    for a, b in zip(pts, pts[1:]):
        total += a[0] * b[1] - b[0] * a[1]
    return total / 2


def square(ox, oy, size, clockwise=True):
    """A closed axis-aligned square ring at the given origin."""
    ring = [
        (ox, oy),
        (ox + size, oy),
        (ox + size, oy + size),
        (ox, oy + size),
        (ox, oy),
    ]
    return ring if clockwise else ring[::-1]


def edges(ring):
    pts = ring if ring[0] == ring[-1] else list(ring) + [ring[0]]
    return list(zip(pts, pts[1:]))


# ---------------------------------------------------------------------------
# The merge: cancel coincident opposite edges, then rebuild rings
# ---------------------------------------------------------------------------

def cancel(all_edges):
    """Annihilate coincident oppositely-directed edges, as a multiset.

    Pairing does not matter: if an edge and its reverse each appear n times,
    all 2n vanish however they are matched up.
    """
    counts = Counter(all_edges)
    for edge in list(counts):
        reverse = (edge[1], edge[0])
        if reverse in counts:
            pairs = min(counts[edge], counts[reverse])
            if pairs:
                counts[edge] -= pairs
                counts[reverse] -= pairs
    return list((+counts).elements())


def reassemble(surviving):
    """Walk the surviving edges back into closed rings.

    This is the step the question is really about. With one ring per region the
    walk is forced; with two coincident opposite rings, a vertex has two
    outgoing edges and the choice is arbitrary.
    """
    outgoing = defaultdict(list)
    for a, b in surviving:
        outgoing[a].append(b)

    rings = []
    remaining = Counter(surviving)
    while sum(remaining.values()):
        start = next(a for (a, _), n in remaining.items() if n)
        ring = [start]
        here = start
        while True:
            nxt = next(
                (b for b in outgoing[here] if remaining[(here, b)]), None
            )
            if nxt is None:
                break  # a dead end: the edge set was not a union of rings
            remaining[(here, nxt)] -= 1
            ring.append(nxt)
            here = nxt
            if here == start:
                break
        rings.append(ring)
    return rings


def drop_collinear(ring):
    """Merge collinear runs, which is what turns two half-sides into one."""
    pts = ring[:-1] if ring[0] == ring[-1] else ring[:]
    out = []
    n = len(pts)
    for i in range(n):
        a, b, c = pts[(i - 1) % n], pts[i], pts[(i + 1) % n]
        cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
        if cross != 0:
            out.append(b)
    return (out + [out[0]]) if out else []


def on_seam(edge, mid):
    """Does this edge lie exactly on one of the two internal seam lines?

    This is the classification 5.4 specifies: only edges on `x = mid` or
    `y = mid` can cancel, because a child's other artificial edges lie on the
    *parent's* boundary, where there is no sibling to cancel against.
    """
    (ax, ay), (bx, by) = edge
    return (ax == mid and bx == mid) or (ay == mid and by == mid)


def merge_four_seam_only(children):
    """Merge as 5.4 actually specifies: cancel only along the seam lines."""
    seam, survivors = [], []
    for (qx, qy), rings in children.items():
        for ring in rings:
            shifted = [(x + qx * E, y + qy * E) for x, y in ring]
            for edge in edges(shifted):
                (seam if on_seam(edge, E) else survivors).append(edge)

    rebuilt = reassemble(cancel(seam) + survivors)
    return [drop_collinear(r) for r in rebuilt if len(r) >= 4]


def drop_zero_area(rings):
    """5.4's closing instruction: drop zero-area rings afterwards.

    This turns out to be what rescues the per-ring representation, which is
    not what it was put there for.
    """
    return [r for r in rings if area(r) != 0]


def merge_four(children):
    """Merge four children, each a list of rings in its own local space.

    `children` is keyed by quadrant. Coordinates are placed into the parent's
    2E space; halving back to E is a separate concern and left out.
    """
    collected = []
    for (qx, qy), rings in children.items():
        for ring in rings:
            shifted = [(x + qx * E, y + qy * E) for x, y in ring]
            collected.extend(edges(shifted))
    rebuilt = reassemble(cancel(collected))
    return [drop_collinear(r) for r in rebuilt if len(r) >= 4]


# ---------------------------------------------------------------------------
# 1. What the winding number says, at any nesting depth
# ---------------------------------------------------------------------------

def winding_at(rings, point):
    """Winding number of a point with respect to a set of oriented rings."""
    total = 0
    for ring in rings:
        pts = ring if ring[0] == ring[-1] else list(ring) + [ring[0]]
        for a, b in zip(pts, pts[1:]):
            if a[1] <= point[1] < b[1] or b[1] <= point[1] < a[1]:
                # Which side of the edge the point falls on.
                t = (point[1] - a[1]) / (b[1] - a[1])
                x = a[0] + t * (b[0] - a[0])
                if x > point[0]:
                    total += 1 if b[1] > a[1] else -1
    return total


def nesting():
    print("1. Net winding decides coverage, whatever the nesting depth")
    print("   (tile is the square 0..E; every ring below swallows it whole)\n")

    centre = (E // 2, E // 2)
    cases = [
        ("bare exterior", [square(-100, -100, E + 200)]),
        (
            "exterior + hole",
            [square(-200, -200, E + 400), square(-100, -100, E + 200, False)],
        ),
        (
            "exterior + hole + inner ring",
            [
                square(-300, -300, E + 600),
                square(-200, -200, E + 400, False),
                square(-100, -100, E + 200),
            ],
        ),
        (
            "four deep",
            [
                square(-400, -400, E + 800),
                square(-300, -300, E + 600, False),
                square(-200, -200, E + 400),
                square(-100, -100, E + 200, False),
            ],
        ),
    ]
    for label, rings in cases:
        w = winding_at(rings, centre)
        naive = len(rings)  # one square per covering ring
        print(f"   {label:<32} winding {w:+d} -> "
              f"{'fill' if w else 'empty'};  "
              f"per-ring would store {naive} coincident square"
              f"{'s' if naive != 1 else ''}")
    print()


# ---------------------------------------------------------------------------
# 2. What the merge does with coincident opposite squares
# ---------------------------------------------------------------------------

def merging():
    print("2. Merging four full tiles\n")

    # (a) The intended representation: one square per fully covered tile.
    children = {q: [square(0, 0, E)] for q in [(0, 0), (1, 0), (0, 1), (1, 1)]}
    parent = merge_four(children)
    print(f"   one square per child   -> {len(parent)} ring(s), "
          f"areas {[area(r) for r in parent]}")
    print(f"       expected one 2E square of area {(2*E)**2}")
    assert len(parent) == 1 and area(parent[0]) == (2 * E) ** 2
    print("       ok: two of each child's four edges cancelled, the eight")
    print("           survivors merged collinearly into one square\n")

    # (b) The representation the question asks about: a coincident pair.
    pair = [square(0, 0, E), square(0, 0, E, clockwise=False)]
    children = {q: list(pair) for q in [(0, 0), (1, 0), (0, 1), (1, 1)]}
    parent = merge_four(children)
    areas = [area(r) for r in parent]
    print(f"   coincident CW + CCW    -> {len(parent)} ring(s), areas {areas}")
    print(f"       net area {sum(areas)} (correct: the pair means 'empty')")
    print(f"       but the rings themselves: {[len(r) for r in parent]} points each")
    for ring in parent:
        print(f"         {ring}")
    print()
    return parent


def spikes(parent):
    print("3. Is the reassembled geometry sane?\n")
    degenerate = [r for r in parent if area(r) == 0]
    print(f"   rings with zero area: {len(degenerate)} of {len(parent)}")
    if degenerate:
        print("   -> reassembly produced zero-area spikes rather than two")
        print("      clean coincident squares. Every vertex of a coincident")
        print("      pair has two outgoing edges, so the walk has an arbitrary")
        print("      choice and takes it. Renders as nothing, which is the")
        print("      right answer, but it is garbage geometry: a decoder")
        print("      checking ring winding has rejected our output for less.")
    else:
        print("   -> two clean coincident squares; net zero, sane rings")
    print()


def seam_only():
    print("4. The same merge, cancelling ONLY along the seams (as 5.4 says)\n")

    quadrants = [(0, 0), (1, 0), (0, 1), (1, 1)]

    children = {q: [square(0, 0, E)] for q in quadrants}
    parent = merge_four_seam_only(children)
    print(f"   one square per child   -> {len(parent)} ring(s), "
          f"areas {[area(r) for r in parent]}")
    assert len(parent) == 1 and area(parent[0]) == (2 * E) ** 2
    print("       ok, same as before\n")

    pair = [square(0, 0, E), square(0, 0, E, clockwise=False)]
    children = {q: list(pair) for q in quadrants}
    parent = merge_four_seam_only(children)
    areas = [area(r) for r in parent]
    print(f"   coincident CW + CCW    -> {len(parent)} ring(s), areas {areas}")
    print(f"       net area {sum(areas)}")
    print("       the parent's own boundary edges are NOT on a seam, so they")
    print("       never cancel -- the pair survives and propagates upward:")
    for ring in parent:
        print(f"         area {area(ring):+.0f}  {ring}")
    print()

    # And the odd-depth case: two CW and one CCW, netting to filled.
    triple = [
        square(0, 0, E),
        square(0, 0, E, clockwise=False),
        square(0, 0, E),
    ]
    children = {q: list(triple) for q in quadrants}
    parent = merge_four_seam_only(children)
    areas = [area(r) for r in parent]
    print(f"   3 coincident (2 CW, 1 CCW) -> {len(parent)} ring(s), areas {areas}")
    print(f"       net area {sum(areas)}, wanted {(2*E)**2}")
    print()

    print("5. After 5.4's 'drop zero-area rings afterwards'\n")
    for label, rings in [
        ("coincident CW + CCW", pair),
        ("3 coincident (2 CW, 1 CCW)", triple),
        ("4 coincident (2 CW, 2 CCW)", pair + pair),
    ]:
        children = {q: list(rings) for q in quadrants}
        kept = drop_zero_area(merge_four_seam_only(children))
        want = (2 * E) ** 2 if sum(1 if area(r) > 0 else -1 for r in rings) else 0
        got = sum(area(r) for r in kept)
        print(f"   {label:<28} -> {len(kept)} ring(s), area {got:.0f}, "
              f"wanted {want}  {'ok' if got == want else 'WRONG'}")
    print()
    return parent


if __name__ == "__main__":
    nesting()
    parent = merging()
    spikes(parent)
    seam_only()
    print("Conclusion")
    print("  A coincident opposite pair is NOT a seam, so the seam sweep never")
    print("  sees it -- which breaks 5.4's stated invariant that within one")
    print("  feature a coincident opposite pair can only be a seam.")
    print()
    print("  Reassembly then mixes the two windings into degenerate rings, and")
    print("  what rescues the net answer is 5.4's 'drop zero-area rings', put")
    print("  there for a different reason. That is luck, not design, and it")
    print("  rests on reassembly happening to produce zero-area junk rather")
    print("  than a plausible wrong ring.")
    print()
    print("  So: decide coverage per FEATURE by net winding and emit AT MOST")
    print("  ONE square. Any nesting depth collapses to 0 or 1, no coincident")
    print("  pair is ever created, and the 5.4 invariant holds as written.")
