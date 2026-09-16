//! Turning a level's measurements into a keep-count.
//!
//! Narrow the keep-count by how far the worst tile is over the limits. Each
//! level feeds the one above, so the factors compound without reasoning about
//! zoom.

use lyv_rank::{Limits, Worst, narrow};

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
    assert_eq!(narrow(50_000, 1.0), 50_000);
}

#[test]
fn a_level_four_times_over_keeps_a_quarter() {
    // The whole rule: keep 1/overflow.
    let worst = Worst {
        bytes: 2_000_000,
        features: 1_000,
    };
    assert_eq!(LIMITS.overflow(worst), 4.0);
    assert_eq!(narrow(40_000, 4.0), 10_000);
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
fn narrowing_never_widens() {
    // Or a feature dropped at one zoom would reappear at a shallower one.
    assert_eq!(
        narrow(1_000, 0.5),
        1_000,
        "an under-full level must not grow"
    );
    assert_eq!(narrow(1_000, 0.0), 1_000);
}

#[test]
fn something_always_survives() {
    // One feature can be over the limit alone, like a coastline at z0. A
    // too-detailed tile beats a blank one.
    assert_eq!(narrow(10, 1_000_000.0), 1);
    assert_eq!(narrow(1, 2.0), 1);
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
