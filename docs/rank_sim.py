import random
random.seed(7)

T = 2000          # non-empty maxzoom tiles
TRIALS = 400

# Features occupying k tiles each. Bigger features span more tiles, which is
# the whole premise: k is a proxy for size that costs nothing to compute.
def trial():
    tiles = [[] for _ in range(T)]
    feats = {}
    fid = 0
    for k in (1, 2, 5, 10, 50, 200):
        for _ in range(30):
            where = random.sample(range(T), k)
            feats[fid] = k
            for t in where:
                tiles[t].append(fid)
            fid += 1
    # pad every tile with small single-tile features so no tile runs dry
    for t in range(T):
        for _ in range(20):
            feats[fid] = 1
            tiles[t].append(fid)
            fid += 1

    # sort each tile's list by size (span) descending
    for t in range(T):
        tiles[t].sort(key=lambda f: -feats[f])

    order = list(range(T)); random.shuffle(order)
    cursor = [0]*T
    ranked = {}
    rank = 0
    progress = True
    while progress:
        progress = False
        for t in order:
            while cursor[t] < len(tiles[t]) and tiles[t][cursor[t]] in ranked:
                cursor[t] += 1
            if cursor[t] < len(tiles[t]):
                ranked[tiles[t][cursor[t]]] = rank
                rank += 1
                progress = True
    return feats, ranked

from collections import defaultdict
pos = defaultdict(list)
for _ in range(TRIALS//40):
    feats, ranked = trial()
    for f, k in feats.items():
        if k in (1,2,5,10,50,200):
            pos[k].append(ranked[f])

print(f"{T} tiles, round 1 = ranks 0..{T-1}")
print(f"{'tiles spanned':>14} {'mean rank':>10} {'predicted (T+1)/(k+1)':>24} {'% in round 1':>13}")
for k in sorted(pos):
    v = pos[k]
    inr1 = 100*sum(1 for p in v if p < T)/len(v)
    print(f"{k:>14} {sum(v)/len(v):>10.0f} {(T+1)/(k+1):>24.0f} {inr1:>12.0f}%")
