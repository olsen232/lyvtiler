//! Our Hilbert numbering against the one PMTiles uses. It must agree exactly,
//! or archives claim an order they don't have. Lives here because
//! `lyv-archive` already depends on `pmtiles`.

use lyv_core::TileCoord;

#[test]
fn our_tile_ids_are_the_ones_pmtiles_assigns() {
    let mut checked = 0;
    for z in 0..=14u32 {
        let side = 1u32 << z;
        // Every tile at small zooms; corners, edges and a diagonal at large
        // ones, where a fold is most likely wrong.
        let picks: Vec<(u32, u32)> = if side <= 16 {
            (0..side)
                .flat_map(|x| (0..side).map(move |y| (x, y)))
                .collect()
        } else {
            let last = side - 1;
            let mut v = vec![
                (0, 0),
                (last, 0),
                (0, last),
                (last, last),
                (side / 2, side / 2),
                (side / 2 - 1, side / 2),
                (side / 2, side / 2 - 1),
            ];
            for i in 0..64 {
                let k = (i * (side / 64).max(1)).min(last);
                v.push((k, k));
                v.push((k, last - k));
                v.push((k, 0));
                v.push((0, k));
            }
            v
        };

        for (x, y) in picks {
            let theirs: u64 = pmtiles::TileId::from(
                pmtiles::TileCoord::new(z as u8, x, y).expect("a valid coordinate"),
            )
            .into();
            let ours = TileCoord { z, x, y }.hilbert_id();
            assert_eq!(ours, theirs, "disagreed at {z}/{x}/{y}");
            checked += 1;
        }
    }
    assert!(checked > 1000, "only checked {checked} coordinates");
}
