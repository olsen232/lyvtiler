#!/usr/bin/env python3
"""Closing clipped chains into rings, checked against Sutherland-Hodgman.

A tile gets only the parts of a ring that fall inside it, which are open
chains. MVT needs closed rings, so the chains are rejoined along the tile's own
edge -- the second half of Weiler-Atherton (design notes §5).

The one thing here that would be a silent disaster if wrong is the *direction*
of the perimeter walk. This script settles it by measurement rather than
argument: Sutherland-Hodgman clips a ring against a rectangle correctly and
simply (it only needs the clip region convex), so it is an independent oracle
for the region the closed rings should cover. It is not what we implement,
because it needs the whole ring per tile -- O(V) per tile, the O(V x T) the
sweep exists to avoid -- but it is exactly what we want to be *equal to*.

Conventions, which matter and are easy to invert:

  - Tile space has y increasing DOWNWARDS, as MVT does.
  - Under the shoelace sum used here, a ring that appears clockwise on screen
    has POSITIVE area. MVT wants exterior rings positive, holes negative.
  - The perimeter walk therefore runs clockwise on screen, parameterised 0..4E
    from the top-left corner. That is asserted against the oracle below, not
    assumed.

Run: python3 docs/close.py
"""

import random

E = 4096  # tile extent


# ---------------------------------------------------------------------------
# Geometry helpers
# ---------------------------------------------------------------------------

def area(ring):
    """Shoelace area. Positive means clockwise on screen (y down)."""
    total = 0.0
    for i in range(len(ring)):
        x0, y0 = ring[i]
        x1, y1 = ring[(i + 1) % len(ring)]
        total += x0 * y1 - x1 * y0
    return total / 2.0


def inside(p):
    return 0 <= p[0] <= E and 0 <= p[1] <= E


def lerp(a, b, t):
    return (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)


def clip_segment(a, b):
    """Liang-Barsky: the sub-interval of [0,1] where the segment is inside."""
    t0, t1 = 0.0, 1.0
    dx, dy = b[0] - a[0], b[1] - a[1]
    for delta, start in ((dx, a[0]), (dy, a[1])):
        for sign, bound in ((-1.0, 0), (1.0, E)):
            num = sign * (bound - start)
            den = sign * delta
            if den == 0:
                if num < 0:
                    return None
                continue
            t = num / den
            if den > 0:
                t1 = min(t1, t)
            else:
                t0 = max(t0, t)
            if t0 > t1:
                return None
    return t0, t1


# ---------------------------------------------------------------------------
# The perimeter, parameterised clockwise on screen from the top-left corner
# ---------------------------------------------------------------------------
#
#   t in [0, E)   top edge,    y = 0, x rising
#   t in [E, 2E)  right edge,  x = E, y rising
#   t in [2E, 3E) bottom edge, y = E, x falling
#   t in [3E, 4E) left edge,   x = 0, y falling

CORNERS = [(0, (0, 0)), (E, (E, 0)), (2 * E, (E, E)), (3 * E, (0, E))]


SNAP = 1e-6


def perimeter_t(p):
    """Parameter of a point known to lie on the tile boundary.

    Snaps first. The prototype cuts in floating point, so an entry computed on
    the x = 0 edge can come back as 5.7e-14; the Rust cut computes each
    crossing as an exact integer on the boundary it sits on, so this snapping
    has no counterpart there.
    """
    x, y = p
    x = 0 if abs(x) < SNAP else (E if abs(x - E) < SNAP else x)
    y = 0 if abs(y) < SNAP else (E if abs(y - E) < SNAP else y)
    # Corners first, so each gets its single canonical parameter.
    if (x, y) == (0, 0):
        return 0.0
    if (x, y) == (E, 0):
        return float(E)
    if (x, y) == (E, E):
        return float(2 * E)
    if (x, y) == (0, E):
        return float(3 * E)
    if y == 0:
        return float(x)
    if x == E:
        return float(E + y)
    if y == E:
        return float(2 * E + (E - x))
    if x == 0:
        return float(3 * E + (E - y))
    raise AssertionError(f"{p} is not on the tile boundary")


def corners_between(t_from, t_to):
    """Corners strictly between two parameters, walking forwards (cyclic)."""
    span = (t_to - t_from) % (4 * E)
    out = []
    for t, point in CORNERS:
        if 0 < (t - t_from) % (4 * E) < span:
            out.append(((t - t_from) % (4 * E), point))
    out.sort()
    return [point for _, point in out]


# ---------------------------------------------------------------------------
# Cutting one ring into the chains that lie inside the tile
# ---------------------------------------------------------------------------

def cut_ring(ring):
    """The parts of `ring` inside the tile, as chains.

    Each chain starts at an entry point and ends at an exit point, both on the
    boundary -- except when the whole ring is inside, which returns it closed.
    """
    if all(inside(p) for p in ring):
        # The rect is convex, so every vertex inside means the whole ring is.
        return [list(ring)], True

    # Start walking from a vertex that is outside, so the first chain begins
    # with an entry and no run wraps around the end of the list. Without this
    # the ring's own start vertex, if it happens to be inside, splits one chain
    # into two -- which is the same splice `cut_ring` needs in Rust.
    start = next(i for i, p in enumerate(ring) if not inside(p))
    rotated = ring[start:] + ring[:start]
    rotated.append(rotated[0])  # close it

    chains = []
    current = None
    for a, b in zip(rotated, rotated[1:]):
        clipped = clip_segment(a, b)
        if clipped is None:
            current = None
            continue
        t0, t1 = clipped
        p0, p1 = lerp(a, b, t0), lerp(a, b, t1)
        if current is None or t0 > 0:
            current = [p0, p1]
            chains.append(current)
        else:
            current.append(p1)
        if t1 < 1:
            current = None
    return [c for c in chains if len(c) >= 2], False


# ---------------------------------------------------------------------------
# The thing under test: closing chains into rings along the tile boundary
# ---------------------------------------------------------------------------

def close_chains(chains):
    """Join open chains into closed rings along the tile perimeter.

    Walks the perimeter forwards (clockwise on screen) from each chain's exit
    to the next chain's entry, emitting the corners passed on the way.
    """
    entries = [perimeter_t(c[0]) for c in chains]
    exits = [perimeter_t(c[-1]) for c in chains]

    used = [False] * len(chains)
    rings = []

    for start in range(len(chains)):
        if used[start]:
            continue
        ring = []
        current = start
        while True:
            used[current] = True
            chain = chains[current]
            # Avoid repeating the point the previous jump landed on.
            ring.extend(chain[1:] if ring and ring[-1] == chain[0] else chain)

            t_exit = exits[current]
            candidates = [
                i for i in range(len(chains)) if not used[i] or i == start
            ]
            # The next entry along the perimeter. `>= 0` rather than `> 0` so a
            # ring that exits and immediately re-enters at the same point pairs
            # with itself rather than travelling the whole way round.
            nxt = min(candidates, key=lambda i: (entries[i] - t_exit) % (4 * E))

            for corner in corners_between(t_exit, entries[nxt]):
                if not ring or ring[-1] != corner:
                    ring.append(corner)

            if nxt == start:
                break
            current = nxt

        # Drop a duplicated closing point; the ring is implicitly closed.
        while len(ring) > 1 and ring[-1] == ring[0]:
            ring.pop()
        if len(ring) >= 3:
            rings.append(ring)
    return rings


# ---------------------------------------------------------------------------
# The oracle: Sutherland-Hodgman against the tile rectangle
# ---------------------------------------------------------------------------

def sutherland_hodgman(ring):
    """Clip a ring to the tile. Correct for any convex clip region."""
    output = list(ring)
    for axis, sign, bound in ((0, 1, 0), (0, -1, E), (1, 1, 0), (1, -1, E)):
        # Keep points where sign * (coord - bound) >= 0.
        def keep(p):
            return sign * (p[axis] - bound) >= 0

        current, output = output, []
        for i in range(len(current)):
            a, b = current[i], current[(i + 1) % len(current)]
            if keep(a):
                output.append(a)
            if keep(a) != keep(b):
                da = sign * (a[axis] - bound)
                db = sign * (b[axis] - bound)
                output.append(lerp(a, b, da / (da - db)))
        if not output:
            return []
    return output


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

def closed_area(ring, is_closed):
    return area(ring)


def check(rings_in, label, verbose=False):
    """Close every ring of one polygon and compare the region to the oracle."""
    chains = []
    passthrough = []
    for ring in rings_in:
        cut, whole = cut_ring(ring)
        if whole:
            passthrough.extend(cut)
        else:
            chains.extend(cut)

    closed = close_chains(chains) + passthrough
    ours = sum(area(r) for r in closed)
    theirs = sum(area(sutherland_hodgman(r)) for r in rings_in)

    for ring in closed:
        for p in ring:
            assert -1e-6 <= p[0] <= E + 1e-6 and -1e-6 <= p[1] <= E + 1e-6, (
                f"{label}: {p} escaped the tile"
            )

    ok = abs(ours - theirs) < max(1.0, abs(theirs) * 1e-9)
    if verbose or not ok:
        print(f"  {'ok ' if ok else 'BAD'} {label}: ours {ours:14.2f}  "
              f"oracle {theirs:14.2f}  rings {len(closed)}")
    return ok


def hand_built():
    print("Hand-built cases")
    cases = []

    # A band crossing the tile top to bottom: the exit of the one chain joins
    # its own entry the long way round, taking two corners with it.
    cases.append(("band through the middle", [
        [(-100, -100), (E // 2, -100), (E // 2, E + 100), (-100, E + 100)],
    ]))

    # A polygon covering a corner, so the walk must emit that corner.
    cases.append(("corner cover", [
        [(-100, -100), (E // 2, -100), (E // 2, E // 2), (-100, E // 2)],
    ]))

    # The tile entirely inside the polygon: no chains at all. The closing step
    # cannot know about this -- it is the coverage scanline's job (5.3) -- so
    # the oracle disagrees, and that is the documented gap rather than a bug.

    # A hole biting into the left edge: the hole stops being a hole and becomes
    # part of the exterior's boundary.
    cases.append(("hole biting the left edge", [
        [(-200, -200), (E + 200, -200), (E + 200, E + 200), (-200, E + 200)],
        [(E // 2, E // 4), (-100, E // 4), (-100, 3 * E // 4), (E // 2, 3 * E // 4)],
    ]))

    # A hole wholly inside the tile, which needs no closing at all, inside an
    # exterior that does cross the tile -- so that the tile is not *entirely*
    # covered, which is the separate case closing cannot see.
    cases.append(("hole wholly inside", [
        [(-200, -200), (3 * E // 4, -200), (3 * E // 4, E + 200), (-200, E + 200)],
        [(1000, 1000), (1000, 2000), (2000, 2000), (2000, 1000)],
    ]))

    # Two separate lobes entering the same edge, so the perimeter walk has to
    # pair the right entry with the right exit.
    cases.append(("two lobes on one edge", [
        [(-100, 500), (1000, 500), (1000, 1500), (-100, 1500)],
        [(-100, 2500), (1000, 2500), (1000, 3500), (-100, 3500)],
    ]))

    failures = 0
    for label, rings in cases:
        if not check(rings, label, verbose=True):
            failures += 1
    return failures


def random_polygons(count=4000, seed=20260910):
    print(f"\n{count} random polygons")
    rng = random.Random(seed)
    failures = 0
    tested = 0
    skipped = 0
    chain_counts = {}
    for case in range(count):
        # Convex-ish rings by sorting angles, so the input is a valid simple
        # polygon and the oracle is trustworthy. Centre and radius are drawn
        # to land the ring on the boundary most of the time.
        n = rng.randint(3, 8)
        cx = rng.uniform(-E * 0.4, E * 1.4)
        cy = rng.uniform(-E * 0.4, E * 1.4)
        radius = rng.uniform(E * 0.1, E * 0.9)
        angles = sorted(rng.uniform(0, 6.283185) for _ in range(n))
        ring = [
            (cx + radius * __import__("math").cos(a),
             cy + radius * __import__("math").sin(a))
            for a in angles
        ]
        if abs(area(ring)) < 1.0:
            skipped += 1
            continue
        # Our convention: exterior rings clockwise on screen, area positive.
        if area(ring) < 0:
            ring.reverse()
        # Skip the fully-covered case, which closing alone cannot see: the
        # ring encloses the tile without ever entering it, so there are no
        # chains to close and no way to know the tile is filled. That is the
        # coverage scanline's job (5.3).
        cut, whole = cut_ring(ring)
        if not cut:
            skipped += 1
            continue

        tested += 1
        key = (len(cut), whole)
        chain_counts[key] = chain_counts.get(key, 0) + 1
        if not check([ring], f"case {case}"):
            failures += 1
            if failures > 5:
                break

    print(f"  tested {tested}, skipped {skipped} (no chains: covered or "
          f"disjoint)")
    print("  chain counts seen (chains, whole-ring): "
          + ", ".join(f"{k}: {v}" for k, v in sorted(chain_counts.items())))
    assert tested > count // 4, f"only {tested} of {count} cases were tested"
    # The interesting cases are the multi-chain ones, where the walk has to
    # pair an exit with another chain's entry rather than its own.
    multi = sum(v for (n, whole), v in chain_counts.items() if n >= 2)
    assert multi > 100, f"only {multi} multi-chain cases; the walk is untested"
    return failures


def orientation_check():
    """The output rings must come out with MVT's winding, not merely the right
    region. A ring with the wrong sign decodes as a hole with no polygon."""
    print("\nOrientation")
    ring = [(-100, -100), (E // 2, -100), (E // 2, E + 100), (-100, E + 100)]
    chains, _ = cut_ring(ring)
    closed = close_chains(chains)
    failures = 0
    for r in closed:
        if area(r) <= 0:
            print(f"  BAD exterior came out with area {area(r)}")
            failures += 1
        else:
            print(f"  ok  exterior area {area(r):.1f} (positive = clockwise "
                  f"on screen = MVT exterior)")

    # And the reverse: if the walk direction were backwards, this is what we
    # would get. Shown so the failure mode is recognisable.
    backwards = [r[::-1] for r in closed]
    print(f"  (a backwards walk would give {area(backwards[0]):.1f})")
    return failures


if __name__ == "__main__":
    failures = hand_built() + random_polygons() + orientation_check()
    print()
    if failures:
        print(f"{failures} FAILURES")
        raise SystemExit(1)
    print("closing agrees with Sutherland-Hodgman on every case")
