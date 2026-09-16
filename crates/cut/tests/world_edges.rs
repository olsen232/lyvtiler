//! Geometry at the very edge of the projected world.
//!
//! Longitude 180 and the southern limit project to `span * extent`, one past
//! the last tile's index. A segment ending there still lands in the last tile,
//! because the cut classifies by midpoint. A lone point there wraps to the
//! first column, or at the southern limit stays a unit inside the last row:
//! MapLibre draws no point at `extent`. Past 180, longitude wraps. A one-vertex
//! linestring is dropped.

use geo_types::Coord;
use lyv_core::Local;
use lyv_core::mercator::project_global;
use lyv_cut::{Pieces, cut_linestring, cut_point};

const EXTENT: u32 = 4096;
const E: Local = EXTENT as Local;
const Z: u32 = 3;

/// The southern limit of the Web Mercator projection.
const SOUTH: f64 = -85.05112877980659;

fn cut(coords: &[(f64, f64)]) -> Pieces {
    let points: Vec<Coord<i64>> = coords
        .iter()
        .map(|&(lon, lat)| {
            let (x, y) = project_global(lon, lat, Z, EXTENT);
            Coord { x, y }
        })
        .collect();
    let mut pieces = Pieces::new();
    cut_linestring(1, &points, Z, EXTENT, &mut pieces);
    pieces
}

/// The single tile a fixture lands in, with its points. Insists there is
/// exactly one.
fn only_tile(coords: &[(f64, f64)]) -> ((u32, u32), Vec<(Local, Local)>) {
    let pieces = cut(coords);
    assert_eq!(
        pieces.len(),
        1,
        "expected one tile, got {:?}",
        pieces.keys()
    );
    let (coord, chains) = pieces.into_iter().next().unwrap();
    assert_eq!(chains.len(), 1, "expected one chain");
    let points = chains[0].points.iter().map(|p| (p.x, p.y)).collect();
    ((coord.x, coord.y), points)
}

#[test]
fn a_line_reaching_the_antimeridian_keeps_its_last_vertex() {
    // Longitude 180 is local x = extent in the last column, not x = 0 of a
    // column that doesn't exist.
    let (address, points) = only_tile(&[(178.0, 0.0), (180.0, 0.0)]);
    assert_eq!(address.0, 7, "the last column at zoom 3");
    assert_eq!(points.last().unwrap().0, E);
}

#[test]
fn a_line_leaving_the_antimeridian_keeps_its_first_vertex() {
    let (address, points) = only_tile(&[(180.0, 0.0), (178.0, 0.0)]);
    assert_eq!(address.0, 7);
    assert_eq!(points.first().unwrap().0, E);
}

#[test]
fn a_line_leaving_the_western_edge_starts_at_zero() {
    let (address, points) = only_tile(&[(-180.0, 0.0), (-178.0, 0.0)]);
    assert_eq!(address.0, 0);
    assert_eq!(points.first().unwrap().0, 0);
}

#[test]
fn a_line_reaching_the_projection_limit_keeps_its_last_vertex() {
    // The same on the other axis.
    let (address, points) = only_tile(&[(0.0, -80.0), (0.0, SOUTH)]);
    assert_eq!(address.1, 7, "the bottom row at zoom 3");
    assert_eq!(points.last().unwrap().1, E);
}

#[test]
fn a_line_reaching_the_northern_limit_keeps_its_last_vertex() {
    let (address, points) = only_tile(&[(0.0, 80.0), (0.0, -SOUTH)]);
    assert_eq!(address.1, 0, "the top row");
    assert_eq!(points.last().unwrap().1, 0);
}

#[test]
fn a_corner_of_the_world_survives() {
    // Both limits at once, where an off-by-one on either axis would show.
    let (address, points) = only_tile(&[(178.0, -80.0), (180.0, SOUTH)]);
    assert_eq!(address, (7, 7));
    assert_eq!(*points.last().unwrap(), (E, E));
}

#[test]
fn no_cut_piece_ever_lands_outside_the_world() {
    // Every tile is addressable and every local coordinate in range.
    let span = 1u32 << Z;
    for lon in [-180.0, -179.9, -90.0, 0.0, 90.0, 179.0, 179.999, 180.0] {
        for lat in [SOUTH, -80.0, 0.0, 80.0, -SOUTH] {
            for pieces in [
                cut(&[(0.0, 0.0), (lon, lat)]),
                cut(&[(lon, lat), (0.0, 0.0)]),
            ] {
                for (coord, chains) in pieces {
                    assert!(
                        coord.x < span && coord.y < span,
                        "{lon},{lat} produced tile {}/{}",
                        coord.x,
                        coord.y
                    );
                    for chain in chains {
                        for point in chain.points {
                            assert!(
                                (0..=E).contains(&point.x) && (0..=E).contains(&point.y),
                                "{lon},{lat} produced {point:?} in {}/{}",
                                coord.x,
                                coord.y
                            );
                        }
                    }
                }
            }
        }
    }
}

/// A point exactly on the world's edge lands where MapLibre will draw it:
/// strictly inside a tile, never at `extent`.
#[test]
fn a_point_on_the_world_edge_lands_inside_a_tile() {
    let span = 1u32 << Z;
    for (lon, lat, expected) in [
        (180.0, 0.0, (0, span / 2)),
        (-180.0, 0.0, (0, span / 2)),
        (0.0, SOUTH, (span / 2, span - 1)),
        (180.0, SOUTH, (0, span - 1)),
    ] {
        let (x, y) = project_global(lon, lat, Z, EXTENT);
        let mut pieces = Pieces::new();
        cut_point(1, Coord { x, y }, Z, EXTENT, &mut pieces);

        let placed: Vec<_> = pieces.iter().collect();
        assert_eq!(
            placed.len(),
            1,
            "{lon},{lat} landed in {} tiles",
            placed.len()
        );
        let (coord, chains) = placed[0];
        assert_eq!((coord.x, coord.y), expected, "{lon},{lat}");
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].points.len(), 1, "a point is one vertex");
        let point = chains[0].points[0];
        assert!(
            (0..E).contains(&point.x) && (0..E).contains(&point.y),
            "{lon},{lat} produced {point:?}"
        );
    }
}

/// A point east or west of the world wraps to the column it would be in; one
/// north or south of it is dropped.
#[test]
fn a_point_past_the_world_edge_wraps_in_longitude_only() {
    let span = i64::from(1u32 << Z);
    let world = span * i64::from(EXTENT);
    let e = i64::from(EXTENT);
    for (at, column, local_x) in [
        (Coord { x: world + 1, y: e }, 0, 1),
        (
            Coord {
                x: world + e + 7,
                y: e,
            },
            1,
            7,
        ),
        (Coord { x: -1, y: e }, span - 1, e - 1),
    ] {
        let mut pieces = Pieces::new();
        cut_point(1, at, Z, EXTENT, &mut pieces);
        let placed: Vec<_> = pieces.iter().collect();
        assert_eq!(placed.len(), 1, "{at:?}");
        let (coord, chains) = placed[0];
        assert_eq!((i64::from(coord.x), coord.y), (column, 1), "{at:?}");
        assert_eq!(i64::from(chains[0].points[0].x), local_x, "{at:?}");
    }

    for at in [Coord { x: 0, y: world + 1 }, Coord { x: 0, y: -1 }] {
        let mut pieces = Pieces::new();
        cut_point(1, at, Z, EXTENT, &mut pieces);
        assert!(pieces.is_empty(), "{at:?} should not be placed anywhere");
    }
}

#[test]
fn a_line_across_the_antimeridian_continues_in_the_first_column() {
    // 178 to 182: the part past 180 is in column 0, meeting the part in the
    // last column at the world edge.
    let pieces = cut(&[(178.0, 0.0), (182.0, 0.0)]);
    let columns: Vec<u32> = pieces.keys().map(|coord| coord.x).collect();
    assert_eq!(columns, vec![0, 7]);
    let last = &pieces[&lyv_core::TileCoord { z: Z, x: 7, y: 4 }][0].points;
    let first = &pieces[&lyv_core::TileCoord { z: Z, x: 0, y: 4 }][0].points;
    assert_eq!(last.last().unwrap().x, E);
    assert_eq!(first.first().unwrap().x, 0);
    assert_eq!(last.last().unwrap().y, first.first().unwrap().y);
}
