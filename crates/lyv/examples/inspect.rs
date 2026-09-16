use lyv_core::{Local, TileCoord};
use lyv_store::{SqliteStore, TileStore};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let store = SqliteStore::open(std::path::Path::new(&path)).unwrap();
    for z in store.zooms().unwrap() {
        let (mut squares, mut rings, mut lines, mut tiles) = (0, 0, 0, 0);
        store
            .for_each_at(z, &mut |_c: TileCoord, data| {
                let e = lyv_mvt::decode_extent(&data).unwrap() as Local;
                let full = [(0, 0), (e, 0), (e, e), (0, e), (0, 0)];
                for chain in lyv_mvt::decode_chains(&data).unwrap() {
                    let pts: Vec<(Local, Local)> =
                        chain.points.iter().map(|p| (p.x, p.y)).collect();
                    if chain.points.first() == chain.points.last() {
                        rings += 1;
                        if pts == full {
                            squares += 1;
                        }
                    } else {
                        lines += 1;
                    }
                }
                tiles += 1;
                Ok(())
            })
            .unwrap();
        println!(
            "  z{z:<2} {tiles:>5} tiles  {rings:>5} rings  {squares:>5} full squares  {lines:>4} open"
        );
    }
}
