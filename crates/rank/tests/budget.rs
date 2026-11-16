//! Turning a level's measurements into a keep-count.
//!
//! How far the worst tile is over the limits says whether a level is thinned;
//! each tile's own cutoff says how far. Each level feeds the one above, so the
//! cuts compound without reasoning about zoom.

use lyv_rank::{Limits, Worst, fitting_cutoff};

const LIMITS: Limits = Limits {
    bytes: 500_000,
    features: 200_000,
};

#[test]
fn a_level_that_fits_is_left_alone() {
    let worst = Worst {
        bytes: 400_000,
        features: 1_000,
    };
    assert_eq!(LIMITS.overflow(worst), 1.0);
    let mut features = [(3, 400_000), (1, 0)];
    assert_eq!(fitting_cutoff(&mut features, 0, LIMITS), None);
}

#[test]
fn a_tile_four_times_over_keeps_what_fits_in_rank_order() {
    let worst = Worst {
        bytes: 2_000_000,
        features: 4,
    };
    assert_eq!(LIMITS.overflow(worst), 4.0);
    // Ranks 0 and 1 fill the budget; rank 2 would break it.
    let mut features = [(2, 500_000), (0, 250_000), (3, 1_000_000), (1, 250_000)];
    assert_eq!(fitting_cutoff(&mut features, 0, LIMITS), Some(2));
}

#[test]
fn whichever_limit_is_worse_decides() {
    // Renderers choke on feature count regardless of size, so either limit
    // can decide.
    let bytes_worse = Worst {
        bytes: 5_000_000,
        features: 400_000,
    };
    assert_eq!(LIMITS.overflow(bytes_worse), 10.0);

    let features_worse = Worst {
        bytes: 600_000,
        features: 2_000_000,
    };
    assert_eq!(LIMITS.overflow(features_worse), 10.0);
}

#[test]
fn something_always_survives() {
    // One feature can be over the limit alone, like a coastline at z0. A
    // too-detailed tile beats a blank one.
    let mut features = [(5, 1_000_000_000)];
    assert_eq!(fitting_cutoff(&mut features, 0, LIMITS), Some(6));
}

#[test]
fn an_empty_level_overflows_nothing() {
    assert_eq!(LIMITS.overflow(Worst::default()), 1.0);
}

#[test]
fn a_limit_of_zero_does_not_divide_by_zero() {
    // `--max-tile-features 0` is a plausible way to ask for no limit.
    let off = Limits {
        bytes: 0,
        features: 0,
    };
    let overflow = off.overflow(Worst {
        bytes: 9_000_000,
        features: 9_000_000,
    });
    assert!(overflow.is_finite(), "got {overflow}");
    assert_eq!(overflow, 1.0, "a zero limit should mean unlimited");
}
