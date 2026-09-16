import random

def shoelace(ring):
    return sum(ring[i][0]*ring[i+1][1] - ring[i+1][0]*ring[i][1]
               for i in range(len(ring)-1)) / 2

def halve(ring):
    return [(round(x/2), round(y/2)) for x, y in ring]

random.seed(7)
flips = []
for _ in range(400_000):
    # A thin quadrilateral somewhere in a maxzoom tile.
    x0 = random.randrange(0, 3000); y0 = random.randrange(0, 3000)
    ring = [(x0, y0),
            (x0, y0 + random.randrange(1, 4)),
            (x0 + random.randrange(20, 900), y0 + random.randrange(1, 4)),
            (x0 + random.randrange(20, 900), y0)]
    ring.append(ring[0])
    start = shoelace(ring)
    if start >= 0:
        continue                      # want a hole (negative) to begin with
    r = ring
    for level in range(1, 7):
        r = halve(r)
        a = shoelace(r)
        if a > 0:                     # sign flipped: hole became an exterior ring
            flips.append((level, start, a, ring, r))
            break

print(f"sign flips found: {len(flips)}")
for level, start, a, orig, got in flips[:3]:
    print(f"\n  after {level} zoom level(s) out:")
    print(f"    hole      area {start:>9.1f}  {orig}")
    print(f"    became    area {a:>9.1f}  {got}   <-- now reads as EXTERIOR")
