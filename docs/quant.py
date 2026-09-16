import random

EXTENT = 4096
ZMAX = 14

def one_shot(w, z):
    "Quantize the true world position directly at zoom z."
    return round(w * EXTENT * (1 << z))

def cascaded(w, z, rounder=round):
    "Quantize once at maxzoom, then halve+requantize down to zoom z."
    c = round(w * EXTENT * (1 << ZMAX))
    for _ in range(ZMAX - z):
        c = rounder(c / 2)
    return c

for name, rounder in [("round-half-even", round), ("truncate (biased)", int)]:
    print(f"\n{name}:")
    print(f"{'zoom':>5} {'max err':>9} {'mean err':>9} {'bits lost':>10}")
    for z in range(ZMAX, -1, -2):
        errs = []
        for _ in range(200_000):
            w = random.random()
            errs.append(cascaded(w, z, rounder) - one_shot(w, z))
        mx = max(abs(e) for e in errs)
        mean = sum(errs) / len(errs)
        print(f"{z:>5} {mx:>9} {mean:>9.3f} {ZMAX - z:>10}")
