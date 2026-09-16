//! The Hilbert numbering tiles are stored in, checked by its shape: levels
//! tile the number line without gaps, and map-adjacent tiles stay close. The
//! oracle test against the `pmtiles` crate is in `lyv-archive`.

use lyv_core::TileCoord;

fn at(z: u32, x: u32, y: u32) -> u64 {
    TileCoord { z, x, y }.hilbert_id()
}

#[test]
fn each_zoom_occupies_the_range_after_the_one_before() {
    // Zoom z holds 4^z ids, starting where z-1 left off; otherwise tiles
    // collide across levels.
    let mut expected_start = 0u64;
    for z in 0..12 {
        let count = 1u64 << (2 * z);
        let side = 1u32 << z;
        let mut ids: Vec<u64> = Vec::new();
        // The whole level for the small zooms; a sample for the larger ones.
        let step = if side > 32 { side / 32 } else { 1 };
        for x in (0..side).step_by(step as usize) {
            for y in (0..side).step_by(step as usize) {
                ids.push(at(z, x, y));
            }
        }
        let low = *ids.iter().min().unwrap();
        let high = *ids.iter().max().unwrap();
        assert!(
            low >= expected_start && high < expected_start + count,
            "zoom {z} produced ids {low}..={high}, outside {expected_start}..{}",
            expected_start + count
        );
        expected_start += count;
    }
}

#[test]
fn a_whole_level_is_a_permutation_of_its_range() {
    // No gaps and no repeats.
    for z in 0..7 {
        let side = 1u32 << z;
        let mut ids: Vec<u64> = (0..side)
            .flat_map(|x| (0..side).map(move |y| at(z, x, y)))
            .collect();
        ids.sort_unstable();
        let start = ((1u64 << (2 * z)) - 1) / 3;
        let want: Vec<u64> = (start..start + (1u64 << (2 * z))).collect();
        assert_eq!(ids, want, "zoom {z} is not a permutation of its range");
    }
}

#[test]
fn the_curve_never_jumps() {
    // Consecutive ids must be map-adjacent, or it's no better than row-major.
    for z in 1..8 {
        let side = 1u32 << z;
        let mut by_id = vec![(0u32, 0u32); (side as usize) * (side as usize)];
        let start = ((1u64 << (2 * z)) - 1) / 3;
        for x in 0..side {
            for y in 0..side {
                by_id[(at(z, x, y) - start) as usize] = (x, y);
            }
        }
        for pair in by_id.windows(2) {
            let (ax, ay) = pair[0];
            let (bx, by) = pair[1];
            let step = ax.abs_diff(bx) + ay.abs_diff(by);
            assert_eq!(
                step, 1,
                "zoom {z}: {:?} to {:?} is not one step",
                pair[0], pair[1]
            );
        }
    }
}

#[test]
fn a_viewport_is_a_short_run_of_ids_where_row_major_is_a_scattering() {
    // The converse doesn't hold, so measure what matters: how much of the file
    // a square viewport spans.
    let z = 8;
    let side = 1u64 << z;
    let block = 4u32;

    let mut worst_hilbert = 0u64;
    let mut worst_row_major = 0u64;
    for bx in (0..1u32 << z).step_by(block as usize) {
        for by in (0..1u32 << z).step_by(block as usize) {
            let cells: Vec<(u32, u32)> = (0..block)
                .flat_map(|dx| (0..block).map(move |dy| (bx + dx, by + dy)))
                .collect();
            let span = |f: &dyn Fn(u32, u32) -> u64| {
                let v: Vec<u64> = cells.iter().map(|&(x, y)| f(x, y)).collect();
                v.iter().max().unwrap() - v.iter().min().unwrap()
            };
            worst_hilbert = worst_hilbert.max(span(&|x, y| at(z, x, y)));
            worst_row_major = worst_row_major.max(span(&|x, y| u64::from(y) * side + u64::from(x)));
        }
    }

    // Aligned to the curve, sixteen tiles are sixteen consecutive ids.
    assert_eq!(
        worst_hilbert, 15,
        "a 4x4 block should be one unbroken run of ids"
    );
    // Row-major spreads the same sixteen over three rows plus three columns.
    assert_eq!(worst_row_major, 3 * side + 3);
    assert!(worst_row_major > worst_hilbert * 40);
}
