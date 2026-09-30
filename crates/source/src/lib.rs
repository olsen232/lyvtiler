//! Reads source data and projects it onto the global integer grid.
//!
//! The only place source files are read. Everything downstream is integers, so
//! the merge can compare coordinates exactly.
//!
//! `maxzoom` here names the grid, not the pyramid: `project_global` spans the
//! world in `extent << z` units, so only the product matters, and projecting
//! onto the deepest grid a run could choose leaves the maxzoom to be picked
//! after the read (§8).
//!
//! GeoJSON for small files and fixtures (parsed whole); FlatGeobuf and
//! GeoParquet for large ones (streamed). GeoJSON's structure is read with
//! `serde_json` and each geometry handed to `geozero`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use geo_types::Coord;

pub use lyv_core::Value;
pub use lyv_core::mercator::Crs;

/// What a read skipped, so the run can say so. Counted apart because they have
/// different fixes: a deeper maxzoom keeps a collapsed feature, nothing brings
/// back a missing geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// Absent or null geometry, and lines of fewer than two points (a line
    /// keeps its vertex count at any resolution).
    pub no_geometry: u64,
    /// Rings that enclosed nothing on the maxzoom grid: slivers narrower than a
    /// unit.
    pub collapsed: u64,
    /// GeometryCollections that lost members: a feature has one kind, so only
    /// the members of the collection's highest dimension are kept.
    pub collection_members: u64,
}

impl Skipped {
    pub fn is_empty(&self) -> bool {
        self.no_geometry == 0 && self.collapsed == 0 && self.collection_members == 0
    }

    /// Record what a feature's geometry lost, and hand back what's left to
    /// keep, if anything is.
    fn keep(&mut self, projected: Projected) -> Option<(Kind, Vec<Vec<Coord<i64>>>)> {
        if projected.dropped_members {
            self.collection_members += 1;
        }
        let Some(kind) = projected.kind else {
            self.no_geometry += 1;
            return None;
        };
        if projected.parts.is_empty() {
            self.count(kind);
            return None;
        }
        Some((kind, projected.parts))
    }

    /// Record a feature left with no parts. Only rings can collapse; an empty
    /// line or point never had geometry.
    fn count(&mut self, kind: Kind) {
        match kind {
            Kind::Rings => self.collapsed += 1,
            Kind::Lines | Kind::Points => self.no_geometry += 1,
        }
    }

    /// A line for the user, or `None` when nothing was lost.
    pub fn report(&self, maxzoom: u32) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if self.collapsed > 0 {
            parts.push(format!(
                "{} too small to draw at zoom {maxzoom}",
                self.collapsed
            ));
        }
        if self.no_geometry > 0 {
            parts.push(format!("{} with no geometry", self.no_geometry));
        }
        let mut line = format!("skipped {}", parts.join(", "));
        if self.collection_members > 0 {
            let lost = format!(
                "{} GeometryCollection{} kept only {} highest-dimension members",
                self.collection_members,
                if self.collection_members == 1 {
                    ""
                } else {
                    "s"
                },
                if self.collection_members == 1 {
                    "its"
                } else {
                    "their"
                },
            );
            line = if parts.is_empty() {
                lost
            } else {
                format!("{line}; {lost}")
            };
        }
        Some(line)
    }
}

/// Whether a feature's parts are points, open lines or closed rings.
///
/// Carried from the source because downstream a line crossing a tile looks
/// exactly like a clipped ring. Points differ in cutting: one tile, no sweep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Points,
    Lines,
    Rings,
}

/// A feature read from a source, projected onto the global grid.
///
/// `parts` is one entry per linestring, or per ring for polygons — all of a
/// feature's rings, exteriors and holes together, consistently oriented (see
/// `orient`). Closing, coverage and merging work on the whole set, and
/// winding says which are holes.
#[derive(Clone, PartialEq, Debug)]
pub struct SourceFeature {
    pub id: u64,
    pub kind: Kind,
    pub parts: Vec<Vec<Coord<i64>>>,
    /// The feature's attributes, sorted by name, so the same data in any format
    /// gives byte-identical tiles. (MVT tags are a map; order means nothing.)
    pub properties: Vec<(String, Value)>,
}

/// What to read from a source. Narrows the read, so unwanted attributes are
/// never parsed or carried.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub attributes: Attributes,
    /// The geometry column to read, for a GeoParquet file with more than one.
    /// `None` is the file's primary column. Other formats have one geometry
    /// per feature, and refuse a name.
    pub geometry_field: Option<String>,
    /// What the coordinates are, whatever the file declares. Nothing is
    /// reprojected: for a file whose coordinates already are lon/lat or Web
    /// Mercator but whose declared CRS is missing, wrong, or one lyv can't
    /// read. `None` goes by the file.
    pub input_crs: Option<Crs>,
}

/// Which attributes to carry. A name listed that no feature has is an error,
/// so a typo can't silently lose a field, or keep one.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Attributes {
    #[default]
    All,
    /// Only these. Empty for none.
    Only(Vec<String>),
    /// Every one but these.
    Except(Vec<String>),
}

/// The attribute filter, plus which listed names have turned up. Stops
/// tracking once they all have.
struct Wanted {
    listed: std::collections::BTreeSet<String>,
    /// Whether the listed names are the ones kept, or the ones skipped.
    keep_listed: bool,
    /// A kept name counts once it's carried, so asking for a column that can't
    /// be (a date, a second geometry) is an error rather than a missing
    /// field. A skipped name is never carried, so it counts when met. A cell,
    /// since readers hold the filter shared while they walk a feature.
    seen: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl Wanted {
    fn new(selection: &Selection) -> Self {
        let (names, keep_listed): (&[String], bool) = match &selection.attributes {
            Attributes::All => (&[], false),
            Attributes::Only(names) => (names, true),
            Attributes::Except(names) => (names, false),
        };
        Self {
            listed: names.iter().cloned().collect(),
            keep_listed,
            seen: Default::default(),
        }
    }

    /// Whether an attribute of this name should be carried at all.
    fn keeps(&self, name: &str) -> bool {
        let listed = self.listed.contains(name);
        if listed && !self.keep_listed {
            self.saw(name);
        }
        listed == self.keep_listed
    }

    /// Record the kept names a feature carried.
    fn note(&self, properties: &[(String, Value)]) {
        if self.keep_listed {
            for (name, _) in properties {
                self.saw(name);
            }
        }
    }

    fn saw(&self, name: &str) {
        let mut seen = self.seen.borrow_mut();
        if seen.len() < self.listed.len() && !seen.contains(name) {
            seen.insert(name.to_string());
        }
    }

    /// Fail if a listed name was never seen on any feature.
    fn check(&self, path: &Path) -> Result<()> {
        let seen = self.seen.borrow();
        let missing: Vec<&str> = self
            .listed
            .iter()
            .filter(|name| !seen.contains(*name))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        bail!(
            "no feature in {} has {} {}",
            path.display(),
            if missing.len() == 1 {
                "an attribute called"
            } else {
                "attributes called"
            },
            missing.join(", "),
        )
    }
}

/// Refuse `--geometry-field` for a format with one geometry per feature.
fn only_one_geometry(selection: &Selection, path: &Path) -> Result<()> {
    if let Some(name) = &selection.geometry_field {
        bail!(
            "{} has one geometry per feature, so there's no geometry field {name:?} to choose; \
             --geometry-field is for GeoParquet",
            path.display()
        );
    }
    Ok(())
}

/// Read a GeoJSON file onto the global grid for `maxzoom` at `extent`.
///
/// `visit` is called once per feature, in file order: a callback, so large
/// inputs needn't be held.
pub fn read_geojson(
    path: &Path,
    maxzoom: u32,
    extent: u32,
    selection: &Selection,
    expect: &mut dyn FnMut(u64),
    visit: &mut dyn FnMut(SourceFeature) -> Result<()>,
) -> Result<Skipped> {
    only_one_geometry(selection, path)?;
    let wanted = Wanted::new(selection);
    let mut skipped = Skipped::default();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let document: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;

    if document.get("type").and_then(serde_json::Value::as_str) != Some("FeatureCollection") {
        bail!(
            "{} is not a GeoJSON FeatureCollection (found type {:?})",
            path.display(),
            document.get("type").and_then(serde_json::Value::as_str)
        );
    }

    let features = document
        .get("features")
        .and_then(serde_json::Value::as_array)
        .with_context(|| format!("{} has no features array", path.display()))?;

    // Known up front, since the collection is parsed whole.
    expect(features.len() as u64);

    let crs = match selection.input_crs {
        Some(crs) => crs,
        None => geojson_crs(&document, path)?,
    };

    for (sequence, feature) in features.iter().enumerate() {
        // Numeric ids are kept. Others fall back to the sequence: a hash could
        // collide silently, and merging joins pieces by id.
        let id = feature
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(sequence as u64);

        let Some(geometry) = feature.get("geometry").filter(|g| !g.is_null()) else {
            skipped.no_geometry += 1;
            continue;
        };

        let projected = geometry_parts(geometry, crs, maxzoom, extent)
            .with_context(|| format!("in feature {sequence} of {}", path.display()))?;
        let Some((kind, parts)) = skipped.keep(projected) else {
            continue;
        };
        let mut properties = read_properties(feature.get("properties"), &wanted);
        properties.sort_by(|a, b| a.0.cmp(&b.0));
        wanted.note(&properties);
        visit(SourceFeature {
            id,
            kind,
            parts,
            properties,
        })?;
    }
    wanted.check(path)?;
    Ok(skipped)
}

/// Project a GeoJSON geometry into runs of global coordinates, via `geozero`.
fn geometry_parts(
    geometry: &serde_json::Value,
    crs: Crs,
    maxzoom: u32,
    extent: u32,
) -> Result<Projected> {
    use geozero::ToGeo;

    let decoded = geozero::geojson::GeoJson(&geometry.to_string())
        .to_geo()
        .context("reading the geometry")?;
    Ok(parts_of(decoded, crs, maxzoom, extent))
}

/// A geometry projected onto the global grid.
struct Projected {
    /// `None` for an empty GeometryCollection: nothing to say what it was.
    kind: Option<Kind>,
    parts: Vec<Vec<Coord<i64>>>,
    /// Whether a GeometryCollection lost members of a lower dimension than
    /// its highest.
    dropped_members: bool,
}

/// Project a decoded geometry onto the global grid, and say what kind it is.
/// Shared by every format, and whatever a format declares: each feature's
/// geometry is taken as it comes.
///
/// A `Line`, `Rect` or `Triangle` is the linestring or polygon it is. A
/// GeometryCollection keeps only its members of the highest dimension
/// (polygons, then lines, then points), nested collections included, since a
/// feature has one kind; the loss is counted.
fn parts_of(decoded: geo_types::Geometry<f64>, crs: Crs, maxzoom: u32, extent: u32) -> Projected {
    use geo_types::Geometry;

    let project = |line: &geo_types::LineString<f64>| -> Vec<Coord<i64>> {
        line.0
            .iter()
            .map(|c| {
                let (x, y) = crs.project_global(c.x, c.y, maxzoom, extent);
                Coord { x, y }
            })
            .collect()
    };

    let at = |point: &geo_types::Point<f64>| -> Vec<Coord<i64>> {
        let (x, y) = crs.project_global(point.x(), point.y(), maxzoom, extent);
        vec![Coord { x, y }]
    };

    let whole = |kind: Kind, parts: Vec<Vec<Coord<i64>>>| Projected {
        kind: Some(kind),
        parts,
        dropped_members: false,
    };

    match decoded {
        // One single-coordinate part per point; a MultiPoint is one feature
        // with several parts.
        Geometry::Point(point) => whole(Kind::Points, vec![at(&point)]),
        Geometry::MultiPoint(points) => whole(Kind::Points, points.0.iter().map(at).collect()),
        Geometry::LineString(line) => whole(Kind::Lines, keep_lines(vec![project(&line)])),
        Geometry::MultiLineString(lines) => whole(
            Kind::Lines,
            keep_lines(lines.0.iter().map(project).collect()),
        ),
        Geometry::Polygon(polygon) => whole(Kind::Rings, rings_of(&polygon, &project)),
        Geometry::MultiPolygon(polygons) => {
            let mut rings = Vec::new();
            for polygon in &polygons.0 {
                rings.extend(rings_of(polygon, &project));
            }
            whole(Kind::Rings, rings)
        }
        Geometry::Line(line) => whole(
            Kind::Lines,
            keep_lines(vec![project(&geo_types::LineString::from(vec![
                line.start, line.end,
            ]))]),
        ),
        Geometry::Rect(rect) => whole(Kind::Rings, rings_of(&rect.to_polygon(), &project)),
        Geometry::Triangle(triangle) => {
            whole(Kind::Rings, rings_of(&triangle.to_polygon(), &project))
        }
        Geometry::GeometryCollection(collection) => {
            let members: Vec<Projected> = collection
                .0
                .into_iter()
                .map(|member| parts_of(member, crs, maxzoom, extent))
                .collect();
            let rank = |kind: Kind| match kind {
                Kind::Points => 0,
                Kind::Lines => 1,
                Kind::Rings => 2,
            };
            let highest = members
                .iter()
                .filter_map(|m| m.kind)
                .max_by_key(|k| rank(*k));
            let mut out = Projected {
                kind: highest,
                parts: Vec::new(),
                dropped_members: false,
            };
            for member in members {
                out.dropped_members |= member.dropped_members;
                match member.kind {
                    Some(kind) if Some(kind) == highest => out.parts.extend(member.parts),
                    Some(_) => out.dropped_members = true,
                    None => {}
                }
            }
            out
        }
    }
}

/// Drop one-point linestring parts: not a run, and MVT can't express one.
/// Promoting them to points would change how they're styled.
fn keep_lines(parts: Vec<Vec<Coord<i64>>>) -> Vec<Vec<Coord<i64>>> {
    parts.into_iter().filter(|part| part.len() >= 2).collect()
}

/// Every ring of one polygon, projected and oriented, exterior first.
fn rings_of(
    polygon: &geo_types::Polygon<f64>,
    project: &impl Fn(&geo_types::LineString<f64>) -> Vec<Coord<i64>>,
) -> Vec<Vec<Coord<i64>>> {
    let mut out = Vec::new();
    // Roles come from the polygon structure, not the winding, which is what
    // we don't trust.
    if let Some(ring) = orient(project(polygon.exterior()), true) {
        out.push(ring);
    }
    for interior in polygon.interiors() {
        if let Some(ring) = orient(project(interior), false) {
            out.push(ring);
        }
    }
    out
}

/// Force a ring's winding, closing it if open. Exteriors get positive area
/// (clockwise on screen, as MVT wants), holes negative.
///
/// Files in the wild often get this wrong, and everything downstream depends
/// on it: getting it backwards yields the exact complement of the region.
/// Only the net sign is checked, which is meaningless for a self-intersecting
/// ring; see `closing_would_lose_it`.
///
/// `None` for a ring with no area.
fn orient(mut ring: Vec<Coord<i64>>, clockwise: bool) -> Option<Vec<Coord<i64>>> {
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    if ring.len() < 4 {
        return None;
    }
    let area = double_area(&ring);
    if area == 0 {
        return None;
    }
    if (area > 0) != clockwise {
        ring.reverse();
    }
    Some(ring)
}

/// Doubled shoelace area. Positive is clockwise on screen, consistent with MVT.
fn double_area(closed: &[Coord<i64>]) -> i128 {
    closed
        .windows(2)
        .map(|w| i128::from(w[0].x) * i128::from(w[1].y) - i128::from(w[1].x) * i128::from(w[0].y))
        .sum()
}

/// Refuse a source whose CRS can't be read, saying how to fix it.
///
/// EPSG:4326 and 3857 reach the grid by arithmetic; anything else needs a
/// projection database lyv doesn't carry. Refusing matters because an NZTM
/// easting of 1,749,000 is a well-formed longitude, and would silently land in
/// the Gulf of Guinea.
fn unreadable_crs(path: &Path, named: &str) -> anyhow::Error {
    let suffix = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("fgb");
    anyhow::anyhow!(
        "{source} is in {named}, but lyv reads only EPSG:4326 (lon/lat in degrees) \
         and EPSG:3857 (Web Mercator in metres).\nReproject it first:\n    \
         ogr2ogr -t_srs EPSG:4326 reprojected.{suffix} {source}\n\
         Or, if its coordinates already are one of those, say which: \
         --input-crs EPSG:4326",
        source = path.display(),
    )
}

/// The CRS an authority and code name, or [`unreadable_crs`]. OGC:CRS84 is the
/// one non-EPSG name accepted.
fn crs_of(authority: &str, code: i64, path: &Path) -> Result<Crs> {
    if authority.eq_ignore_ascii_case("OGC") && (code == 84 || code == 1) {
        return Ok(Crs::Wgs84);
    }
    if !authority.eq_ignore_ascii_case("EPSG") {
        return Err(unreadable_crs(path, &format!("{authority}:{code}")));
    }
    Crs::from_epsg(code).ok_or_else(|| unreadable_crs(path, &format!("EPSG:{code}")))
}

/// A CRS given by name, for [`Selection::input_crs`]: `EPSG:4326`,
/// `OGC:CRS84` or `EPSG:3857`, in any spelling a file could use.
pub fn parse_crs(name: &str) -> Result<Crs> {
    authority_and_code(name)
        .and_then(|(authority, code)| {
            if authority.eq_ignore_ascii_case("OGC") && code == 84 {
                Some(Crs::Wgs84)
            } else if authority.eq_ignore_ascii_case("EPSG") {
                Crs::from_epsg(code)
            } else {
                None
            }
        })
        .with_context(|| {
            format!(
                "{name:?} isn't a CRS lyv reads: it reads EPSG:4326 (or OGC:CRS84), \
                 lon/lat in degrees, and EPSG:3857, Web Mercator in metres"
            )
        })
}

/// The authority and code in a CRS name: `EPSG:3857`,
/// `urn:ogc:def:crs:EPSG::3857`, or OGC's `CRS84` (`CRS84h` with a height
/// axis, which lyv ignores).
fn authority_and_code(name: &str) -> Option<(String, i64)> {
    let name = name.trim();
    let upper = name.to_ascii_uppercase();
    if upper.ends_with("CRS84") || upper.ends_with("CRS84H") || upper == "CRS:84" {
        return Some(("OGC".to_string(), 84));
    }

    // Dropping empty fields collapses the URN's `::`, leaving the authority
    // just before the code.
    let fields: Vec<&str> = name.split(':').filter(|field| !field.is_empty()).collect();
    let code = fields.last()?.parse::<i64>().ok()?;
    let authority = fields.get(fields.len().checked_sub(2)?)?;
    Some((authority.to_string(), code))
}

/// The CRS a GeoJSON document declares. RFC 7946 fixed GeoJSON at WGS84, so
/// usually there's nothing to read; a pre-RFC `crs` member is still honoured.
fn geojson_crs(document: &serde_json::Value, path: &Path) -> Result<Crs> {
    let Some(crs) = document.get("crs").filter(|crs| !crs.is_null()) else {
        return Ok(Crs::Wgs84);
    };
    let Some(name) = crs
        .pointer("/properties/name")
        .and_then(serde_json::Value::as_str)
    else {
        // The removed `link` form, or something homemade: refuse, don't guess.
        return Err(unreadable_crs(path, &crs.to_string()));
    };
    let (authority, code) = authority_and_code(name).ok_or_else(|| unreadable_crs(path, name))?;
    crs_of(&authority, code, path)
}

/// The CRS a GeoParquet column declares, as PROJJSON. Absent means OGC:CRS84
/// by the spec, and null means unknown; both read as lon/lat.
fn geoparquet_crs(crs: Option<&serde_json::Value>, path: &Path) -> Result<Crs> {
    let Some(crs) = crs.filter(|crs| !crs.is_null()) else {
        return Ok(Crs::Wgs84);
    };

    // PROJJSON's `id` holds authority and code. The code may be a number, a
    // number in a string, or a word: OGC's lon/lat is `CRS84`. Read as the
    // name `authority:code`, which handles all three.
    let id = crs.get("id");
    let authority = id
        .and_then(|id| id.get("authority"))
        .and_then(serde_json::Value::as_str);
    let code = id
        .and_then(|id| id.get("code"))
        .and_then(|code| match code {
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::String(text) => Some(text.clone()),
            _ => None,
        });
    let named = authority
        .zip(code)
        .and_then(|(authority, code)| authority_and_code(&format!("{authority}:{code}")));

    match named {
        Some((authority, code)) => crs_of(&authority, code, path),
        // No `id`: a CRS defined by parameters, which needs a projection
        // database to identify. Refuse.
        _ => Err(unreadable_crs(
            path,
            crs.get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("a CRS with no EPSG id"),
        )),
    }
}

/// The CRS a FlatGeobuf header declares. The field is optional and often empty,
/// so absent means lon/lat rather than an error.
fn flatgeobuf_crs(header: &flatgeobuf::Header<'_>, path: &Path) -> Result<Crs> {
    let Some(crs) = header.crs() else {
        return Ok(Crs::Wgs84);
    };
    // `code` is 0 when unset.
    if crs.code() == 0 {
        return Ok(Crs::Wgs84);
    }
    crs_of(crs.org().unwrap_or("EPSG"), i64::from(crs.code()), path)
}

/// Read a FlatGeobuf file onto the global grid, one feature at a time. Same
/// contract as [`read_geojson`].
pub fn read_flatgeobuf(
    path: &Path,
    maxzoom: u32,
    extent: u32,
    selection: &Selection,
    expect: &mut dyn FnMut(u64),
    visit: &mut dyn FnMut(SourceFeature) -> Result<()>,
) -> Result<Skipped> {
    use flatgeobuf::{FallibleStreamingIterator, FgbReader};
    use geozero::{FeatureProperties, ToGeo};

    only_one_geometry(selection, path)?;
    let wanted = Wanted::new(selection);
    let mut skipped = Skipped::default();

    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut features = FgbReader::open(&mut reader)
        .with_context(|| format!("{} is not a FlatGeobuf file", path.display()))?
        .select_all()
        .with_context(|| format!("reading the features of {}", path.display()))?;

    // The header carries the count, so progress can say "of" from the start.
    if let Some(total) = features.features_count() {
        expect(total as u64);
    }

    // Checked before reading any feature.
    let crs = match selection.input_crs {
        Some(crs) => crs,
        None => flatgeobuf_crs(&features.header(), path)?,
    };

    let mut sequence = 0u64;
    while let Some(feature) = features
        .next()
        .with_context(|| format!("reading feature {sequence} of {}", path.display()))?
    {
        // A feature may have no geometry, which `to_geo` would report as
        // malformed and stop the run. Asked directly, so a corrupt geometry
        // still fails.
        if feature.geometry().is_none() {
            skipped.no_geometry += 1;
            sequence += 1;
            continue;
        }

        let geometry = feature
            .to_geo()
            .with_context(|| format!("in feature {sequence} of {}", path.display()))?;

        let Some((kind, parts)) = skipped.keep(parts_of(geometry, crs, maxzoom, extent)) else {
            sequence += 1;
            continue;
        };

        // FlatGeobuf has no feature ids, so the position in the file is used.
        let mut collector = Properties::new(&wanted);
        feature
            .process_properties(&mut collector)
            .with_context(|| format!("reading the attributes of feature {sequence}"))?;

        let mut properties = collector.kept;
        properties.sort_by(|a, b| a.0.cmp(&b.0));
        wanted.note(&properties);

        visit(SourceFeature {
            id: sequence,
            kind,
            parts,
            properties,
        })?;
        sequence += 1;
    }
    wanted.check(path)?;
    Ok(skipped)
}

/// Collects a feature's attributes as they're walked. Filtered here, so an
/// unwanted attribute never becomes a `Value`.
struct Properties<'a> {
    kept: Vec<(String, Value)>,
    wanted: &'a Wanted,
}

impl<'a> Properties<'a> {
    fn new(wanted: &'a Wanted) -> Self {
        Self {
            kept: Vec::new(),
            wanted,
        }
    }
}

impl geozero::PropertyProcessor for Properties<'_> {
    fn property(
        &mut self,
        _index: usize,
        name: &str,
        value: &geozero::ColumnValue,
    ) -> geozero::error::Result<bool> {
        use geozero::ColumnValue as Column;
        if !self.wanted.keeps(name) {
            return Ok(false);
        }
        // Narrowed to what MVT can carry. Every integer width becomes `Int`
        // or `Uint`, since MVT has no narrower type and a style could not tell
        // them apart anyway. `Binary` is dropped: there is nothing sensible to
        // turn bytes into, and a stringified blob would only bloat the tile.
        let carried = match value {
            Column::Byte(v) => Value::Int(i64::from(*v)),
            Column::UByte(v) => Value::Uint(u64::from(*v)),
            Column::Bool(v) => Value::Bool(*v),
            Column::Short(v) => Value::Int(i64::from(*v)),
            Column::UShort(v) => Value::Uint(u64::from(*v)),
            Column::Int(v) => Value::Int(i64::from(*v)),
            Column::UInt(v) => Value::Uint(u64::from(*v)),
            Column::Long(v) => Value::Int(*v),
            Column::ULong(v) => Value::Uint(*v),
            Column::Float(v) => Value::Float(*v),
            Column::Double(v) => Value::Double(*v),
            Column::String(v) | Column::Json(v) | Column::DateTime(v) => {
                Value::Str((*v).to_string())
            }
            Column::Binary(_) => return Ok(false),
        };
        self.kept.push((name.to_string(), carried));
        Ok(false)
    }
}

/// Read a GeoParquet file, projecting every feature onto the global grid.
///
/// The same contract as [`read_geojson`], and the same reason for supporting
/// it as FlatGeobuf: it is a format real data arrives in, read a batch at a
/// time rather than parsed whole.
///
/// Geometries are asked for in their **GeoArrow-native** form rather than left
/// as WKB, so that both of GeoParquet's encodings arrive here looking the
/// same — 1.0 stores WKB and 1.1 may store either, and `geoparquet` will parse
/// the one into the other on the way past.
///
/// Attributes come from every column that is not the geometry, which is what
/// makes this shorter than the other two readers: Parquet has already sorted
/// out the types, so there is no property walker and no JSON coercion.
pub fn read_geoparquet(
    path: &Path,
    maxzoom: u32,
    extent: u32,
    selection: &Selection,
    expect: &mut dyn FnMut(u64),
    visit: &mut dyn FnMut(SourceFeature) -> Result<()>,
) -> Result<Skipped> {
    let wanted = Wanted::new(selection);
    use geoparquet::reader::{GeoParquetReaderBuilder, GeoParquetRecordBatchReader};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let mut skipped = Skipped::default();
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("{} is not a Parquet file", path.display()))?;

    // Known from the footer, so this costs nothing — the same free "of" a
    // FlatGeobuf header gives.
    expect(builder.metadata().file_metadata().num_rows().max(0) as u64);

    let geo = builder
        .geoparquet_metadata()
        .transpose()
        .with_context(|| format!("reading the GeoParquet metadata of {}", path.display()))?
        .with_context(|| {
            format!(
                "{} is Parquet but not GeoParquet: it has no `geo` metadata, so nothing \
                 says which column is the geometry",
                path.display()
            )
        })?;
    // The primary column unless told otherwise. GeoParquet lists every
    // geometry column in its metadata, so a name that isn't one is refused
    // here, with the ones there are.
    let geometry_column = selection
        .geometry_field
        .clone()
        .unwrap_or_else(|| geo.primary_column.clone());
    let Some(column) = geo.columns.get(&geometry_column) else {
        let mut names: Vec<&str> = geo.columns.keys().map(String::as_str).collect();
        names.sort_unstable();
        bail!(
            "{} has no geometry column {geometry_column:?}; its geometry columns are {}",
            path.display(),
            names.join(", ")
        );
    };

    let schema = target_schema(&builder, &geo, &geometry_column)
        .with_context(|| format!("reading the geometry schema of {}", path.display()))?;

    // Read from the metadata rather than per batch: GeoParquet requires every
    // chunk of a column to share one CRS, and this way a refusal comes before
    // the first row is decoded.
    let crs = match selection.input_crs {
        Some(crs) => crs,
        None => geoparquet_crs(column.crs.as_ref(), path)?,
    };

    let reader = builder
        .build()
        .with_context(|| format!("opening {}", path.display()))?;
    let reader = GeoParquetRecordBatchReader::try_new(reader, schema)
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    let mut sequence = 0u64;
    for batch in reader {
        let batch = batch.with_context(|| format!("reading a batch of {}", path.display()))?;
        let index = batch
            .schema()
            .index_of(&geometry_column)
            .with_context(|| format!("{} has no column {geometry_column:?}", path.display()))?;

        let geometries = geoarrow_geometries(batch.column(index), batch.schema().field(index))
            .with_context(|| format!("reading the geometry of {}", path.display()))?;
        let properties = arrow_properties(&batch, index, &wanted);

        for (row, geometry) in geometries.into_iter().enumerate() {
            let Some(geometry) = geometry else {
                skipped.no_geometry += 1;
                sequence += 1;
                continue;
            };
            let Some((kind, parts)) = skipped.keep(parts_of(geometry, crs, maxzoom, extent)) else {
                sequence += 1;
                continue;
            };
            let mut properties: Vec<(String, Value)> = properties
                .iter()
                .filter_map(|(name, values)| values[row].clone().map(|value| (name.clone(), value)))
                .collect();
            properties.sort_by(|a, b| a.0.cmp(&b.0));
            wanted.note(&properties);

            // Parquet has no feature ids, so the row number is used.
            visit(SourceFeature {
                id: sequence,
                kind,
                parts,
                properties,
            })?;
            sequence += 1;
        }
    }
    wanted.check(path)?;
    Ok(skipped)
}

/// The schema to read a GeoParquet file into: `geometry_column` tagged as
/// geometry, every other column as stored.
///
/// A WKB column stays WKB, decoded row by row, whatever its `geometry_types`
/// declares. That declaration can be wrong (a "Polygon" column holding
/// MultiPolygons), and parsing to the single native type it names fails on the
/// first row that differs. Staying WKB is also the faster read: every row ends
/// up as `geo_types`, and a native array would be a second copy on the way
/// (vicmap, 3.5M polygons: 3.9 s against 5.0 s for the mixed native type).
/// A native column is its declared type by construction. Other geometry
/// columns aren't parsed at all: lyv never reads them, and a wrong declaration
/// there would fail the read too.
fn target_schema(
    builder: &parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder<std::fs::File>,
    geo: &geoparquet::metadata::GeoParquetMetadata,
    geometry_column: &str,
) -> Result<arrow_schema::SchemaRef> {
    use geoarrow_schema::{GeoArrowType, Metadata, WkbType};
    use geoparquet::metadata::GeoParquetColumnEncoding;
    use geoparquet::reader::GeoParquetReaderBuilder;

    let stored = builder.schema();
    let native = builder
        .geoarrow_schema(geo, true, Default::default())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let column = &geo.columns[geometry_column];

    let fields: Vec<arrow_schema::FieldRef> = stored
        .fields()
        .iter()
        .zip(native.fields())
        .map(|(stored, native)| {
            if stored.name() != geometry_column {
                stored.clone()
            } else if column.encoding == GeoParquetColumnEncoding::WKB {
                let metadata = std::sync::Arc::new(Metadata::from(column.clone()));
                let wkb = GeoArrowType::Wkb(WkbType::new(metadata));
                std::sync::Arc::new(wkb.to_field(stored.name(), stored.is_nullable()))
            } else {
                native.clone()
            }
        })
        .collect();
    Ok(std::sync::Arc::new(
        arrow_schema::Schema::new_with_metadata(fields, stored.metadata().clone()),
    ))
}

/// Every geometry in a GeoArrow column, as `geo_types`.
///
/// Needs the reader's decorated field: which GeoArrow type a column holds is in
/// the field's extension metadata, not its Arrow data type.
fn geoarrow_geometries(
    column: &std::sync::Arc<dyn arrow_array::Array>,
    field: &arrow_schema::Field,
) -> Result<Vec<Option<geo_types::Geometry<f64>>>> {
    use geoarrow_array::{GeoArrowArrayAccessor, downcast_geoarrow_array};

    fn collect<'a>(
        array: &'a impl GeoArrowArrayAccessor<'a>,
    ) -> geoarrow_schema::error::GeoArrowResult<Vec<Option<geo_types::Geometry<f64>>>> {
        use geo_traits::to_geo::ToGeoGeometry;
        // `None` for a null, `Err` for an undecodable row.
        array
            .iter()
            .map(|item| match item {
                None => Ok(None),
                Some(geom) => geom.map(|geom| Some(geom.to_geometry())),
            })
            .collect()
    }

    let array = geoarrow_array::array::from_arrow_array(column.as_ref(), field)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let array = array.as_ref();
    downcast_geoarrow_array!(array, collect).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Every non-geometry column, as a list of values per column. Types MVT can't
/// carry are skipped, not stringified.
fn arrow_properties(
    batch: &arrow_array::RecordBatch,
    geometry: usize,
    wanted: &Wanted,
) -> Vec<(String, Vec<Option<Value>>)> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::*;

    let mut out = Vec::new();
    for (index, field) in batch.schema().fields().iter().enumerate() {
        if index == geometry {
            continue;
        }
        // An unwanted column is never decoded.
        if !wanted.keeps(field.name()) {
            continue;
        }
        let column = batch.column(index);
        let take = |f: &dyn Fn(usize) -> Value| -> Vec<Option<Value>> {
            (0..column.len())
                .map(|row| (!column.is_null(row)).then(|| f(row)))
                .collect()
        };
        let values = match field.data_type() {
            arrow_schema::DataType::Utf8 => {
                let a = column.as_string::<i32>();
                take(&|row| Value::Str(a.value(row).to_string()))
            }
            arrow_schema::DataType::LargeUtf8 => {
                let a = column.as_string::<i64>();
                take(&|row| Value::Str(a.value(row).to_string()))
            }
            arrow_schema::DataType::Boolean => {
                let a = column.as_boolean();
                take(&|row| Value::Bool(a.value(row)))
            }
            arrow_schema::DataType::Float32 => {
                let a = column.as_primitive::<Float32Type>();
                take(&|row| Value::Float(a.value(row)))
            }
            arrow_schema::DataType::Float64 => {
                let a = column.as_primitive::<Float64Type>();
                take(&|row| Value::Double(a.value(row)))
            }
            arrow_schema::DataType::Int8 => {
                let a = column.as_primitive::<Int8Type>();
                take(&|row| Value::Int(i64::from(a.value(row))))
            }
            arrow_schema::DataType::Int16 => {
                let a = column.as_primitive::<Int16Type>();
                take(&|row| Value::Int(i64::from(a.value(row))))
            }
            arrow_schema::DataType::Int32 => {
                let a = column.as_primitive::<Int32Type>();
                take(&|row| Value::Int(i64::from(a.value(row))))
            }
            arrow_schema::DataType::Int64 => {
                let a = column.as_primitive::<Int64Type>();
                take(&|row| Value::Int(a.value(row)))
            }
            arrow_schema::DataType::UInt8 => {
                let a = column.as_primitive::<UInt8Type>();
                take(&|row| Value::Uint(u64::from(a.value(row))))
            }
            arrow_schema::DataType::UInt16 => {
                let a = column.as_primitive::<UInt16Type>();
                take(&|row| Value::Uint(u64::from(a.value(row))))
            }
            arrow_schema::DataType::UInt32 => {
                let a = column.as_primitive::<UInt32Type>();
                take(&|row| Value::Uint(u64::from(a.value(row))))
            }
            arrow_schema::DataType::UInt64 => {
                let a = column.as_primitive::<UInt64Type>();
                take(&|row| Value::Uint(a.value(row)))
            }
            // Dates, lists, structs and so on: not MVT scalars.
            _ => continue,
        };
        out.push((field.name().clone(), values));
    }
    out
}

/// Read a source file, choosing the reader by extension: `.fgb`, `.parquet`,
/// and GeoJSON for anything else.
pub fn read(
    path: &Path,
    maxzoom: u32,
    extent: u32,
    selection: &Selection,
    expect: &mut dyn FnMut(u64),
    visit: &mut dyn FnMut(SourceFeature) -> Result<()>,
) -> Result<Skipped> {
    let ends_with = |want: &str| {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(want))
    };
    if ends_with("fgb") {
        read_flatgeobuf(path, maxzoom, extent, selection, expect, visit)
    } else if ends_with("parquet") {
        read_geoparquet(path, maxzoom, extent, selection, expect, visit)
    } else {
        read_geojson(path, maxzoom, extent, selection, expect, visit)
    }
}

/// Turn a GeoJSON `properties` object into MVT values:
///
/// - a number is an integer if it is one and fits, a double otherwise;
/// - `null` is dropped: MVT has no null, and an empty string would make
///   `["has", "name"]` true;
/// - arrays and objects become their JSON text;
/// - names the [`Selection`] doesn't want are dropped before conversion.
fn read_properties(
    properties: Option<&serde_json::Value>,
    wanted: &Wanted,
) -> Vec<(String, Value)> {
    let Some(object) = properties.and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };

    object
        .iter()
        .filter(|(key, _)| wanted.keeps(key))
        .filter_map(|(key, value)| Some((key.clone(), to_value(value)?)))
        .collect()
}

fn to_value(value: &serde_json::Value) -> Option<Value> {
    use serde_json::Value as Json;
    Some(match value {
        Json::Null => return None,
        Json::Bool(b) => Value::Bool(*b),
        Json::String(s) => Value::Str(s.clone()),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                Value::Uint(u)
            } else {
                Value::Double(n.as_f64()?)
            }
        }
        nested @ (Json::Array(_) | Json::Object(_)) => Value::Str(nested.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyv_core::mercator::project_global;

    const EXTENT: u32 = 4096;
    const Z: u32 = 5;

    fn read(json: &str) -> Result<Vec<SourceFeature>> {
        Ok(read_counting(json)?.0)
    }

    fn read_counting(json: &str) -> Result<(Vec<SourceFeature>, Skipped)> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.geojson");
        std::fs::write(&path, json).unwrap();

        let mut out = Vec::new();
        let skipped = read_geojson(
            &path,
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |feature| {
                out.push(feature);
                Ok(())
            },
        )?;
        Ok((out, skipped))
    }

    fn read_selecting(json: &str, selection: &Selection) -> Result<Vec<SourceFeature>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.geojson");
        std::fs::write(&path, json).unwrap();
        let mut out = Vec::new();
        crate::read(&path, 12, EXTENT, selection, &mut |_| {}, &mut |feature| {
            out.push(feature);
            Ok(())
        })?;
        Ok(out)
    }

    const TWO: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"a":1,"b":2,"c":3},
         "geometry":{"type":"Point","coordinates":[174.7,-41.3]}},
        {"type":"Feature","properties":{"a":4,"d":5},
         "geometry":{"type":"Point","coordinates":[174.8,-41.2]}}]}"#;

    fn names(feature: &SourceFeature) -> Vec<&str> {
        feature.properties.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn skipped_attributes_are_left_out_and_the_rest_kept() {
        let selection = Selection {
            attributes: Attributes::Except(vec!["b".into(), "d".into()]),
            ..Selection::default()
        };
        let features = read_selecting(TWO, &selection).unwrap();
        assert_eq!(names(&features[0]), vec!["a", "c"]);
        assert_eq!(names(&features[1]), vec!["a"]);
    }

    #[test]
    fn skipping_an_attribute_no_feature_has_is_an_error() {
        // A typo would otherwise keep the field it meant to drop.
        let selection = Selection {
            attributes: Attributes::Except(vec!["b".into(), "nonesuch".into()]),
            ..Selection::default()
        };
        let said = format!("{:#}", read_selecting(TWO, &selection).unwrap_err());
        assert!(said.contains("nonesuch") && !said.contains("b,"), "{said}");
    }

    #[test]
    fn a_geometry_field_is_refused_where_there_is_only_one_geometry() {
        let selection = Selection {
            geometry_field: Some("geom".into()),
            ..Selection::default()
        };
        let said = format!("{:#}", read_selecting(TWO, &selection).unwrap_err());
        assert!(
            said.contains("--geometry-field is for GeoParquet"),
            "{said}"
        );
    }

    #[test]
    fn a_feature_too_small_for_the_grid_is_counted_as_collapsed() {
        // A sliver narrower than a unit at maxzoom can't be drawn and is
        // dropped, but the run must say how many, in case a deeper maxzoom is
        // wanted.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[1,0],[2,0],[0,0]]]}}"#,
        );
        let (features, skipped) = read_counting(&json).unwrap();
        assert_eq!(features, Vec::new());
        assert_eq!(skipped.collapsed, 1);
        assert_eq!(skipped.no_geometry, 0);
    }

    #[test]
    fn a_null_geometry_is_counted_separately_from_a_collapse() {
        // Counted apart: only a collapse suggests a deeper maxzoom.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":null},
               {"type":"Feature","properties":{},"geometry":null}"#,
        );
        let (features, skipped) = read_counting(&json).unwrap();
        assert_eq!(features, Vec::new());
        assert_eq!(skipped.no_geometry, 2);
        assert_eq!(skipped.collapsed, 0);
    }

    #[test]
    fn a_file_that_loses_nothing_reports_nothing() {
        let (features, skipped) = read_counting(&collection(&line("[[0,0],[1,1]]"))).unwrap();
        assert_eq!(features.len(), 1);
        assert!(skipped.is_empty(), "got {skipped:?}");
    }

    fn collection(features: &str) -> String {
        format!(r#"{{"type":"FeatureCollection","features":[{features}]}}"#)
    }

    fn line(coords: &str) -> String {
        format!(
            r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"LineString","coordinates":{coords}}}}}"#
        )
    }

    fn global(lon: f64, lat: f64) -> Coord<i64> {
        let (x, y) = project_global(lon, lat, Z, EXTENT);
        Coord { x, y }
    }

    #[test]
    fn a_linestring_is_read_and_projected() {
        let features = read(&collection(&line("[[0,0],[10,20]]"))).unwrap();

        assert_eq!(features.len(), 1);
        assert_eq!(
            features[0].parts,
            vec![vec![global(0.0, 0.0), global(10.0, 20.0)]]
        );
    }

    #[test]
    fn projection_matches_the_mercator_crate() {
        // The source must project with `mercator`, not arithmetic of its own.
        let features = read(&collection(&line("[[174.7762,-41.2865],[174.8,-41.3]]"))).unwrap();
        assert_eq!(
            features[0].parts[0],
            vec![global(174.7762, -41.2865), global(174.8, -41.3)]
        );
    }

    #[test]
    fn a_multilinestring_becomes_several_parts_of_one_feature() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":
               {"type":"MultiLineString","coordinates":[[[0,0],[1,1]],[[5,5],[6,6]]]}}"#,
        );
        let features = read(&json).unwrap();

        assert_eq!(features.len(), 1, "one feature, not two");
        assert_eq!(features[0].parts.len(), 2, "with two parts");
    }

    #[test]
    fn features_without_an_id_are_numbered_in_file_order() {
        let json = collection(&format!(
            "{},{},{}",
            line("[[0,0],[1,1]]"),
            line("[[2,2],[3,3]]"),
            line("[[4,4],[5,5]]")
        ));
        let ids: Vec<u64> = read(&json).unwrap().iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn a_numeric_id_in_the_source_is_kept() {
        let json = collection(
            r#"{"type":"Feature","id":12345,"properties":{},"geometry":
               {"type":"LineString","coordinates":[[0,0],[1,1]]}}"#,
        );
        assert_eq!(read(&json).unwrap()[0].id, 12345);
    }

    #[test]
    fn a_non_numeric_id_falls_back_to_the_sequence() {
        // Merging needs a u64; hashing a string id could collide silently.
        let json = collection(
            r#"{"type":"Feature","id":"waikato-river","properties":{},"geometry":
               {"type":"LineString","coordinates":[[0,0],[1,1]]}}"#,
        );
        assert_eq!(read(&json).unwrap()[0].id, 0);
    }

    #[test]
    fn elevations_are_ignored_rather_than_rejected() {
        // GeoJSON positions may carry a third element.
        let features = read(&collection(&line("[[0,0,123.4],[10,20,567.8]]"))).unwrap();
        assert_eq!(
            features[0].parts,
            vec![vec![global(0.0, 0.0), global(10.0, 20.0)]]
        );
    }

    #[test]
    fn an_empty_collection_yields_nothing() {
        assert_eq!(read(&collection("")).unwrap(), Vec::new());
    }

    #[test]
    fn a_one_point_linestring_is_skipped() {
        // Skipped rather than rejecting the whole file.
        let features = read(&collection(&line("[[5,5]]"))).unwrap();
        assert_eq!(features, Vec::new());
    }

    #[test]
    fn a_null_geometry_is_skipped() {
        let json = collection(r#"{"type":"Feature","properties":{},"geometry":null}"#);
        assert_eq!(read(&json).unwrap(), Vec::new());
    }

    #[test]
    fn a_geometry_collection_keeps_its_highest_dimension() {
        // A feature has one kind: the polygon stays, the point and line go, and
        // that's counted rather than silent. Nesting is flattened.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":
               {"type":"GeometryCollection","geometries":[
                 {"type":"Point","coordinates":[1,2]},
                 {"type":"GeometryCollection","geometries":[
                   {"type":"LineString","coordinates":[[0,0],[1,1]]},
                   {"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}]}]}}"#,
        );
        let (features, skipped) = read_counting(&json).unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].kind, Kind::Rings);
        assert_eq!(skipped.collection_members, 1);
        assert!(skipped.report(12).unwrap().contains("GeometryCollection"));
    }

    #[test]
    fn a_collection_of_one_kind_loses_nothing() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":
               {"type":"GeometryCollection","geometries":[
                 {"type":"Point","coordinates":[1,2]},
                 {"type":"Point","coordinates":[3,4]}]}}"#,
        );
        let (features, skipped) = read_counting(&json).unwrap();
        assert_eq!(features[0].kind, Kind::Points);
        assert_eq!(features[0].parts.len(), 2);
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn an_empty_collection_has_no_geometry() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":
               {"type":"GeometryCollection","geometries":[]}}"#,
        );
        let (features, skipped) = read_counting(&json).unwrap();
        assert!(features.is_empty());
        assert_eq!(skipped.no_geometry, 1);
    }

    // -----------------------------------------------------------------------
    // FlatGeobuf
    // -----------------------------------------------------------------------

    /// Convert GeoJSON to FlatGeobuf with the reference writer, so the fixture
    /// isn't our own idea of the format.
    fn to_fgb(json: &str, geometry_type: flatgeobuf::GeometryType) -> tempfile::TempPath {
        use flatgeobuf::FgbWriter;
        use geozero::GeozeroDatasource;
        use geozero::geojson::GeoJsonReader;

        let mut fgb = FgbWriter::create("test", geometry_type).unwrap();
        let mut bytes = json.as_bytes();
        GeoJsonReader(&mut bytes).process(&mut fgb).unwrap();

        let file = tempfile::NamedTempFile::with_suffix(".fgb").unwrap();
        let (handle, path) = file.into_parts();
        fgb.write(std::io::BufWriter::new(handle)).unwrap();
        path
    }

    fn read_fgb(path: &Path) -> Vec<SourceFeature> {
        let mut out = Vec::new();
        read_flatgeobuf(
            path,
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |f| {
                out.push(f);
                Ok(())
            },
        )
        .unwrap();
        out
    }

    /// Attributes as a sorted set; the formats order them differently.
    fn sorted_properties(feature: &SourceFeature) -> Vec<(String, Value)> {
        let mut out = feature.properties.clone();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn a_flatgeobuf_linestring_reads_the_same_as_the_geojson_it_came_from() {
        // Two readers, one answer.
        let json = collection(
            r#"{"type":"Feature","properties":{"name":"a river","length":118},
               "geometry":{"type":"LineString",
                "coordinates":[[174.0,-41.0],[175.0,-40.0],[176.0,-41.5]]}}"#,
        );
        let fgb = to_fgb(&json, flatgeobuf::GeometryType::LineString);

        let from_json = read(&json).unwrap();
        let from_fgb = read_fgb(&fgb);

        assert_eq!(from_fgb.len(), 1);
        assert_eq!(from_fgb[0].kind, Kind::Lines);
        assert_eq!(
            from_fgb[0].parts, from_json[0].parts,
            "the two readers projected the geometry differently"
        );
        assert_eq!(
            sorted_properties(&from_fgb[0]),
            sorted_properties(&from_json[0])
        );
    }

    #[test]
    fn a_flatgeobuf_polygon_is_oriented_the_same_way() {
        // Winding is imposed whichever reader the geometry came through.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[
                 [[0,0],[0,3],[3,3],[3,0],[0,0]],
                 [[1,1],[1,2],[2,2],[2,1],[1,1]]
               ]}}"#,
        );
        let fgb = to_fgb(&json, flatgeobuf::GeometryType::Polygon);

        let from_json = read(&json).unwrap();
        let from_fgb = read_fgb(&fgb);

        assert_eq!(from_fgb[0].kind, Kind::Rings);
        assert_eq!(from_fgb[0].parts.len(), 2, "exterior and hole");
        assert!(double_area(&from_fgb[0].parts[0]) > 0, "exterior positive");
        assert!(double_area(&from_fgb[0].parts[1]) < 0, "hole negative");
        assert_eq!(from_fgb[0].parts, from_json[0].parts);
    }

    #[test]
    fn flatgeobuf_attribute_types_survive() {
        // Every typed integer width collapses to MVT's one.
        let json = collection(
            r#"{"type":"Feature","properties":{
                 "name":"x","count":78,"depth":2.5,"tidal":false
               },"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}"#,
        );
        let fgb = to_fgb(&json, flatgeobuf::GeometryType::LineString);
        let got = sorted_properties(&read_fgb(&fgb)[0]);

        let names: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["count", "depth", "name", "tidal"]);
        assert!(matches!(got[0].1, Value::Int(78) | Value::Uint(78)));
        assert_eq!(got[1].1, Value::Double(2.5));
        assert_eq!(got[2].1, Value::Str("x".into()));
        assert_eq!(got[3].1, Value::Bool(false));
    }

    #[test]
    fn flatgeobuf_features_are_numbered_in_file_order() {
        // No ids in the format, so position is used.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"LineString",
               "coordinates":[[0,0],[1,1]]}},
               {"type":"Feature","properties":{},"geometry":{"type":"LineString",
               "coordinates":[[2,2],[3,3]]}}"#,
        );
        let fgb = to_fgb(&json, flatgeobuf::GeometryType::LineString);
        let ids: Vec<u64> = read_fgb(&fgb).iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn the_reader_is_chosen_by_extension() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"LineString",
               "coordinates":[[174,-41],[175,-40]]}}"#,
        );
        let fgb = to_fgb(&json, flatgeobuf::GeometryType::LineString);

        let mut count = 0;
        super::read(
            &fgb,
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |_| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 1, ".fgb should go to the FlatGeobuf reader");

        let file = tempfile::NamedTempFile::with_suffix(".geojson").unwrap();
        std::fs::write(file.path(), &json).unwrap();
        let mut count = 0;
        super::read(
            file.path(),
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |_| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 1, ".geojson should go to the GeoJSON reader");
    }

    #[test]
    fn a_file_that_is_not_flatgeobuf_is_refused_with_its_path() {
        let file = tempfile::NamedTempFile::with_suffix(".fgb").unwrap();
        std::fs::write(file.path(), b"not a flatgeobuf file at all").unwrap();
        let error = super::read(
            file.path(),
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |_| Ok(()),
        )
        .expect_err("should refuse");
        let message = format!("{error:#}");
        assert!(
            message.contains("FlatGeobuf") && message.contains(".fgb"),
            "should say what it wanted and which file, got: {message}"
        );
    }

    // -----------------------------------------------------------------------
    // Coordinate reference systems
    // -----------------------------------------------------------------------

    /// The same as [`to_fgb`], but with a CRS written into the header.
    fn to_fgb_in_crs(
        json: &str,
        geometry_type: flatgeobuf::GeometryType,
        org: &str,
        code: i32,
    ) -> tempfile::TempPath {
        use flatgeobuf::{FgbCrs, FgbWriter, FgbWriterOptions};
        use geozero::GeozeroDatasource;
        use geozero::geojson::GeoJsonReader;

        let mut fgb = FgbWriter::create_with_options(
            "test",
            geometry_type,
            FgbWriterOptions {
                crs: FgbCrs {
                    org: Some(org),
                    code,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let mut bytes = json.as_bytes();
        GeoJsonReader(&mut bytes).process(&mut fgb).unwrap();

        let file = tempfile::NamedTempFile::with_suffix(".fgb").unwrap();
        let (handle, path) = file.into_parts();
        fgb.write(std::io::BufWriter::new(handle)).unwrap();
        path
    }

    fn try_read_fgb(path: &Path) -> Result<Vec<SourceFeature>> {
        let mut out = Vec::new();
        read_flatgeobuf(
            path,
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |f| {
                out.push(f);
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// Wellington in EPSG:3857, from the reference formula, not the code under
    /// test.
    fn in_metres(lon: f64, lat: f64) -> (f64, f64) {
        const R: f64 = 6_378_137.0;
        (
            R * lon.to_radians(),
            R * ((45.0 + lat / 2.0).to_radians().tan()).ln(),
        )
    }

    #[test]
    fn a_flatgeobuf_in_web_mercator_lands_where_the_same_place_in_degrees_does() {
        // The same place written both ways reaches the same grid square.
        let (lon, lat) = (174.777, -41.288);
        let (east, north) = in_metres(lon, lat);

        let degrees = to_fgb(
            &collection(&format!(
                r#"{{"type":"Feature","properties":{{}},
                   "geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#
            )),
            flatgeobuf::GeometryType::Point,
        );
        let metres = to_fgb_in_crs(
            &collection(&format!(
                r#"{{"type":"Feature","properties":{{}},
                   "geometry":{{"type":"Point","coordinates":[{east},{north}]}}}}"#
            )),
            flatgeobuf::GeometryType::Point,
            "EPSG",
            3857,
        );

        let from_degrees = read_fgb(&degrees);
        let from_metres = try_read_fgb(&metres).expect("EPSG:3857 should be readable");

        let here = from_degrees[0].parts[0][0];
        let there = from_metres[0].parts[0][0];
        assert!(
            (here.x - there.x).abs() <= 1 && (here.y - there.y).abs() <= 1,
            "{here:?} in degrees but {there:?} in metres"
        );
    }

    #[test]
    fn a_flatgeobuf_in_a_crs_that_needs_a_datum_shift_is_refused() {
        // An NZTM easting of 1,749,000 would otherwise be read as a longitude.
        let fgb = to_fgb_in_crs(
            &collection(
                r#"{"type":"Feature","properties":{},
                   "geometry":{"type":"Point","coordinates":[1749000,5427000]}}"#,
            ),
            flatgeobuf::GeometryType::Point,
            "EPSG",
            2193,
        );

        let error = try_read_fgb(&fgb).expect_err("EPSG:2193 should be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("2193"),
            "should name the CRS it found, got: {message}"
        );
        assert!(
            message.contains("4326") && message.contains("3857"),
            "and the two it reads, got: {message}"
        );
        assert!(
            message.contains("ogr2ogr"),
            "and how to fix it, got: {message}"
        );
    }

    #[test]
    fn an_input_crs_overrides_the_one_a_file_declares() {
        // GDA2020 is lon/lat, within two metres of WGS 84: refused as declared,
        // read as lon/lat when told to.
        let point = r#"{"type":"Feature","properties":{},
           "geometry":{"type":"Point","coordinates":[174.7,-41.3]}}"#;
        let fgb = to_fgb_in_crs(
            &collection(point),
            flatgeobuf::GeometryType::Point,
            "EPSG",
            7844,
        );
        let refused = format!(
            "{:#}",
            try_read_fgb(&fgb).expect_err("EPSG:7844 as declared")
        );
        assert!(
            refused.contains("--input-crs"),
            "should offer the flag: {refused}"
        );

        let mut read = Vec::new();
        read_flatgeobuf(
            &fgb,
            Z,
            EXTENT,
            &Selection {
                input_crs: Some(Crs::Wgs84),
                ..Selection::default()
            },
            &mut |_| {},
            &mut |f| {
                read.push(f);
                Ok(())
            },
        )
        .unwrap();
        let (x, y) = lyv_core::mercator::project_global(174.7, -41.3, Z, EXTENT);
        assert_eq!(read[0].parts[0][0], geo_types::Coord { x, y });

        // And the same for GeoJSON's declaration, to Web Mercator.
        let json = r#"{"type":"FeatureCollection",
           "crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::2193"}},
           "features":[{"type":"Feature","properties":{},
             "geometry":{"type":"Point","coordinates":[0,0]}}]}"#;
        let selection = Selection {
            input_crs: Some(Crs::WebMercator),
            ..Selection::default()
        };
        let features = read_selecting(json, &selection).unwrap();
        let (x, y) = lyv_core::mercator::project_global(0.0, 0.0, 12, EXTENT);
        assert_eq!(features[0].parts[0][0], geo_types::Coord { x, y });
    }

    #[test]
    fn an_input_crs_is_named_as_a_file_would_name_it() {
        for (name, crs) in [
            ("EPSG:4326", Crs::Wgs84),
            ("epsg:4326", Crs::Wgs84),
            ("OGC:CRS84", Crs::Wgs84),
            ("CRS84", Crs::Wgs84),
            ("urn:ogc:def:crs:EPSG::3857", Crs::WebMercator),
            ("EPSG:3857", Crs::WebMercator),
        ] {
            assert_eq!(parse_crs(name).unwrap(), crs, "{name}");
        }
        for name in ["EPSG:7844", "4326", "wgs84", ""] {
            let said = format!("{:#}", parse_crs(name).unwrap_err());
            assert!(
                said.contains("EPSG:4326") && said.contains("EPSG:3857"),
                "{name}: {said}"
            );
        }
    }

    #[test]
    fn a_flatgeobuf_with_no_crs_is_read_as_lon_lat() {
        // Most writers leave the field empty, so absent can't be an error.
        let json = collection(
            r#"{"type":"Feature","properties":{},
               "geometry":{"type":"Point","coordinates":[174.777,-41.288]}}"#,
        );
        let unlabelled = to_fgb(&json, flatgeobuf::GeometryType::Point);
        let labelled = to_fgb_in_crs(&json, flatgeobuf::GeometryType::Point, "EPSG", 4326);

        assert_eq!(
            try_read_fgb(&unlabelled).unwrap()[0].parts,
            try_read_fgb(&labelled).unwrap()[0].parts,
        );
    }

    #[test]
    fn a_geojson_that_declares_web_mercator_is_read_in_metres() {
        // A pre-RFC `crs` member saying metres is believed.
        let (lon, lat) = (174.777, -41.288);
        let (east, north) = in_metres(lon, lat);
        let json = format!(
            r#"{{"type":"FeatureCollection",
               "crs":{{"type":"name","properties":{{"name":"urn:ogc:def:crs:EPSG::3857"}}}},
               "features":[{{"type":"Feature","properties":{{}},
                 "geometry":{{"type":"Point","coordinates":[{east},{north}]}}}}]}}"#
        );

        let from_metres = read(&json).unwrap();
        let from_degrees = read(&collection(&format!(
            r#"{{"type":"Feature","properties":{{}},
               "geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#
        )))
        .unwrap();

        let here = from_degrees[0].parts[0][0];
        let there = from_metres[0].parts[0][0];
        assert!(
            (here.x - there.x).abs() <= 1 && (here.y - there.y).abs() <= 1,
            "{here:?} in degrees but {there:?} in metres"
        );
    }

    #[test]
    fn a_geojson_that_declares_a_crs_needing_a_datum_shift_is_refused() {
        let json = r#"{"type":"FeatureCollection",
           "crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::2193"}},
           "features":[{"type":"Feature","properties":{},
             "geometry":{"type":"Point","coordinates":[1749000,5427000]}}]}"#;
        let message = format!("{:#}", read(json).expect_err("should refuse"));
        assert!(
            message.contains("2193") && message.contains("ogr2ogr"),
            "should name it and say how to fix it, got: {message}"
        );
    }

    #[test]
    fn the_spellings_a_writer_uses_for_a_crs_all_parse() {
        assert_eq!(
            authority_and_code("EPSG:3857"),
            Some(("EPSG".to_string(), 3857))
        );
        assert_eq!(
            authority_and_code("urn:ogc:def:crs:EPSG::4326"),
            Some(("EPSG".to_string(), 4326))
        );
        // OGC's lon/lat is spelled, not numbered.
        assert_eq!(
            authority_and_code("urn:ogc:def:crs:OGC:1.3:CRS84"),
            Some(("OGC".to_string(), 84))
        );
        assert_eq!(authority_and_code("CRS84"), Some(("OGC".to_string(), 84)));
        // With a height axis, which lyv ignores.
        assert_eq!(
            authority_and_code("OGC:CRS84h"),
            Some(("OGC".to_string(), 84))
        );
    }

    #[test]
    fn a_geoparquet_column_is_read_from_its_projjson_id() {
        let path = Path::new("test.parquet");
        let wgs84 = serde_json::json!({"id": {"authority": "EPSG", "code": 4326}});
        let mercator = serde_json::json!({"id": {"authority": "EPSG", "code": 3857}});
        // Some writers put the code in a string.
        let as_text = serde_json::json!({"id": {"authority": "EPSG", "code": "3857"}});
        let nztm = serde_json::json!({
            "name": "NZGD2000 / New Zealand Transverse Mercator 2000",
            "id": {"authority": "EPSG", "code": 2193},
        });

        assert_eq!(geoparquet_crs(Some(&wgs84), path).unwrap(), Crs::Wgs84);
        assert_eq!(
            geoparquet_crs(Some(&mercator), path).unwrap(),
            Crs::WebMercator
        );
        assert_eq!(
            geoparquet_crs(Some(&as_text), path).unwrap(),
            Crs::WebMercator
        );
        assert!(geoparquet_crs(Some(&nztm), path).is_err());

        // OGC's lon/lat has a word for a code: GeoParquet's own default,
        // written out, as GDAL does.
        for code in ["CRS84", "CRS84h"] {
            let crs84 = serde_json::json!({
                "name": "WGS 84 (CRS84)",
                "id": {"authority": "OGC", "code": code},
            });
            assert_eq!(
                geoparquet_crs(Some(&crs84), path).unwrap(),
                Crs::Wgs84,
                "{code}"
            );
        }

        // Absent means CRS84 by the spec; null means unknown, read the same.
        assert_eq!(geoparquet_crs(None, path).unwrap(), Crs::Wgs84);
        assert_eq!(
            geoparquet_crs(Some(&serde_json::Value::Null), path).unwrap(),
            Crs::Wgs84
        );
    }

    // -----------------------------------------------------------------------
    // Attributes
    // -----------------------------------------------------------------------

    fn props(json: &str) -> Vec<(String, Value)> {
        let doc = collection(json);
        read(&doc).unwrap().into_iter().next().unwrap().properties
    }

    const LINE: &str = r#""geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}"#;

    #[test]
    fn attributes_come_through_with_their_types() {
        let got = props(&format!(
            r#"{{"type":"Feature","properties":{{
                 "name":"Wairarapa","count":78,"depth":2.5,"tidal":false
               }},{LINE}}}"#
        ));
        assert_eq!(
            got,
            vec![
                ("count".to_string(), Value::Int(78)),
                ("depth".to_string(), Value::Double(2.5)),
                ("name".to_string(), Value::Str("Wairarapa".into())),
                ("tidal".to_string(), Value::Bool(false)),
            ],
            "a whole number should not become a double, nor a double an integer"
        );
    }

    #[test]
    fn a_null_attribute_is_dropped_rather_than_emptied() {
        // An empty string would make `["has", "name"]` true.
        let got = props(&format!(
            r#"{{"type":"Feature","properties":{{"name":null,"kind":"lake"}},{LINE}}}"#
        ));
        assert_eq!(got, vec![("kind".to_string(), Value::Str("lake".into()))]);
    }

    #[test]
    fn a_nested_attribute_is_kept_as_its_json_text() {
        // Keeping the text beats losing the data silently.
        let got = props(&format!(
            r#"{{"type":"Feature","properties":{{"tags":["a","b"],"at":{{"x":1}}}},{LINE}}}"#
        ));
        assert_eq!(
            got,
            vec![
                ("at".to_string(), Value::Str(r#"{"x":1}"#.into())),
                ("tags".to_string(), Value::Str(r#"["a","b"]"#.into())),
            ]
        );
    }

    #[test]
    fn a_feature_with_no_properties_object_is_still_read() {
        let doc = collection(&format!(r#"{{"type":"Feature",{LINE}}}"#));
        let features = read(&doc).unwrap();
        assert_eq!(features.len(), 1);
        assert!(features[0].properties.is_empty());
    }

    #[test]
    fn a_number_too_large_for_i64_stays_a_number() {
        let got = props(&format!(
            r#"{{"type":"Feature","properties":{{"big":18446744073709551615}},{LINE}}}"#
        ));
        assert_eq!(
            got,
            vec![("big".to_string(), Value::Uint(18_446_744_073_709_551_615))]
        );
    }

    // -----------------------------------------------------------------------
    // Polygons
    // -----------------------------------------------------------------------

    fn double_area_of(ring: &[Coord<i64>]) -> i128 {
        double_area(ring)
    }

    #[test]
    fn a_polygon_arrives_as_rings_with_the_exterior_clockwise_on_screen() {
        // Right-hand rule: anticlockwise in lon/lat, which projection flips to
        // clockwise on screen. No reversing needed.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#,
        );
        let features = read(&json).unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].kind, Kind::Rings);
        assert_eq!(features[0].parts.len(), 1);

        let ring = &features[0].parts[0];
        assert_eq!(ring.first(), ring.last(), "a ring arrives closed");
        assert!(
            double_area_of(ring) > 0,
            "the exterior should be clockwise on screen, got area {}",
            double_area_of(ring)
        );
    }

    #[test]
    fn an_exterior_wound_the_wrong_way_is_corrected() {
        // The same square wound the wrong way: winding is imposed, not trusted.
        let right_way = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#,
        );
        let wrong_way = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[0,1],[1,1],[1,0],[0,0]]]}}"#,
        );
        assert_eq!(
            read(&right_way).unwrap()[0].parts,
            read(&wrong_way).unwrap()[0].parts,
            "both windings should produce the same oriented ring"
        );
    }

    #[test]
    fn a_hole_comes_back_wound_against_its_exterior() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[
                 [[0,0],[3,0],[3,3],[0,3],[0,0]],
                 [[1,1],[2,1],[2,2],[1,2],[1,1]]
               ]}}"#,
        );
        let parts = &read(&json).unwrap()[0].parts;
        assert_eq!(parts.len(), 2, "exterior then hole");
        assert!(double_area_of(&parts[0]) > 0, "exterior positive");
        assert!(double_area_of(&parts[1]) < 0, "hole negative");
    }

    #[test]
    fn a_hole_wound_the_same_way_as_its_exterior_is_corrected() {
        // Both rings clockwise in lon/lat; the hole's role comes from the
        // structure, so it's flipped.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[
                 [[0,0],[0,3],[3,3],[3,0],[0,0]],
                 [[1,1],[1,2],[2,2],[2,1],[1,1]]
               ]}}"#,
        );
        let parts = &read(&json).unwrap()[0].parts;
        assert_eq!(parts.len(), 2);
        assert!(double_area_of(&parts[0]) > 0, "exterior positive");
        assert!(double_area_of(&parts[1]) < 0, "hole negative");
    }

    #[test]
    fn a_multipolygon_flattens_to_all_its_rings_under_one_id() {
        // Four rings, one feature, one id, so the pieces can be rejoined.
        let json = collection(
            r#"{"type":"Feature","id":9,"properties":{},"geometry":
               {"type":"MultiPolygon","coordinates":[
                 [[[0,0],[1,0],[1,1],[0,1],[0,0]]],
                 [[[5,5],[8,5],[8,8],[5,8],[5,5]],
                  [[6,6],[7,6],[7,7],[6,7],[6,6]]]
               ]}}"#,
        );
        let features = read(&json).unwrap();
        assert_eq!(features.len(), 1, "one feature, not two");
        assert_eq!(features[0].id, 9);
        assert_eq!(features[0].parts.len(), 3, "two exteriors and one hole");

        let signs: Vec<bool> = features[0]
            .parts
            .iter()
            .map(|r| double_area_of(r) > 0)
            .collect();
        assert_eq!(signs, vec![true, true, false]);
    }

    #[test]
    fn a_ring_with_no_area_is_dropped_rather_than_given_an_orientation() {
        // A collapsed ring is dropped, and a polygon left with nothing is
        // skipped. The fixture runs along a parallel, because only there does
        // lon/lat collinearity survive Mercator exactly.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[1,0],[2,0],[0,0]]]}}"#,
        );
        assert_eq!(read(&json).unwrap(), Vec::new());
    }

    #[test]
    fn a_lon_lat_diagonal_is_not_collinear_once_projected() {
        // This ring looks degenerate in lon/lat but isn't once projected, so
        // it survives.
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[1,1],[2,2],[3,3],[1,1]]]}}"#,
        );
        let features = read(&json).unwrap();
        assert_eq!(features.len(), 1, "the projection bends it into a sliver");
        assert_ne!(double_area_of(&features[0].parts[0]), 0);
    }

    #[test]
    fn a_polygon_left_unclosed_by_the_source_is_closed() {
        let json = collection(
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon",
               "coordinates":[[[0,0],[1,0],[1,1],[0,1]]]}}"#,
        );
        let ring = &read(&json).unwrap()[0].parts[0];
        assert_eq!(ring.first(), ring.last());
        assert!(double_area_of(ring) > 0);
    }

    #[test]
    fn malformed_json_is_refused() {
        let error = read("{not json at all").expect_err("should refuse");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn something_that_is_not_a_feature_collection_is_refused() {
        let error = read(r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#)
            .expect_err("should refuse");
        assert!(
            error.to_string().contains("FeatureCollection"),
            "got: {error}"
        );
    }

    #[test]
    fn a_visitor_error_stops_the_read() {
        let json = collection(&format!(
            "{},{},{}",
            line("[[0,0],[1,1]]"),
            line("[[2,2],[3,3]]"),
            line("[[4,4],[5,5]]")
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.geojson");
        std::fs::write(&path, &json).unwrap();

        let mut seen = 0;
        let result = read_geojson(
            &path,
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |_| {
                seen += 1;
                if seen == 2 {
                    bail!("visitor gave up");
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(seen, 2, "and it stops where the visitor did");
    }

    #[test]
    fn a_missing_file_is_reported_with_its_path() {
        let error = read_geojson(
            Path::new("/no/such/file.geojson"),
            Z,
            EXTENT,
            &Selection::default(),
            &mut |_total| {},
            &mut |_| Ok(()),
        )
        .expect_err("should refuse");
        assert!(
            error.to_string().contains("/no/such/file.geojson"),
            "got: {error}"
        );
    }
}
