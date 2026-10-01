//! Reading GeoParquet, from a file the test writes itself: no binary fixture
//! to check in, nothing on anyone's disk.

use std::sync::Arc;

use arrow_array::{Float64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geoarrow_array::GeoArrowArray;
use geoarrow_array::builder::PolygonBuilder;
use geoarrow_schema::{CoordType, Dimension, PolygonType};
use geoparquet::writer::{GeoParquetRecordBatchEncoder, GeoParquetWriterOptions};
use lyv_source::{Kind, Selection, SourceFeature, Value, read};
use parquet::arrow::ArrowWriter;

const Z: u32 = 12;
const EXTENT: u32 = 4096;

/// A square of `side` degrees with its lower-left corner at `(x, y)`.
fn square(x: f64, y: f64, side: f64) -> geo_types::Polygon<f64> {
    geo_types::Polygon::new(
        geo_types::LineString::from(vec![
            (x, y),
            (x + side, y),
            (x + side, y + side),
            (x, y + side),
            (x, y),
        ]),
        vec![],
    )
}

/// Write a GeoParquet file of named, valued squares; return its path.
fn fixture(dir: &std::path::Path, count: usize) -> std::path::PathBuf {
    let polygons: Vec<geo_types::Polygon<f64>> = (0..count)
        .map(|i| square(172.0 + i as f64 * 0.5, -44.0 + i as f64 * 0.3, 0.2))
        .collect();

    let typ =
        PolygonType::new(Dimension::XY, Default::default()).with_coord_type(CoordType::Separated);
    let geometry = PolygonBuilder::from_polygons(&polygons, typ).finish();

    let names: Vec<String> = (0..count).map(|i| format!("square {i}")).collect();
    let values: Vec<f64> = (0..count).map(|i| i as f64 * 1.5).collect();

    let schema = Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("valuation", DataType::Float64, false),
        geometry.data_type().to_field("geometry", true),
    ]);
    let batch = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![
            Arc::new(StringArray::from(names)),
            Arc::new(Float64Array::from(values)),
            geometry.to_array_ref(),
        ],
    )
    .unwrap();

    let path = dir.join("squares.parquet");
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder =
        GeoParquetRecordBatchEncoder::try_new(&schema, &GeoParquetWriterOptions::default())
            .unwrap();
    let mut writer = ArrowWriter::try_new(file, encoder.target_schema(), None).unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
    path
}

fn read_all(path: &std::path::Path) -> (Vec<SourceFeature>, u64) {
    let mut out = Vec::new();
    let mut total = 0;
    read(
        path,
        Z,
        EXTENT,
        &Selection::default(),
        &mut |n| total = n,
        &mut |feature| {
            out.push(feature);
            Ok(())
        },
    )
    .unwrap();
    (out, total)
}

#[test]
fn a_geoparquet_file_is_read_by_its_extension() {
    // Only the suffix says GeoParquet, so the dispatch is part of the feature.
    let dir = tempfile::tempdir().unwrap();
    let (features, _) = read_all(&fixture(dir.path(), 5));
    assert_eq!(features.len(), 5);
    assert!(features.iter().all(|f| f.kind == Kind::Rings));
}

#[test]
fn the_row_count_is_known_before_the_first_feature() {
    // The count is in the footer, so progress knows the total.
    let dir = tempfile::tempdir().unwrap();
    let (features, total) = read_all(&fixture(dir.path(), 7));
    assert_eq!(total, 7);
    assert_eq!(features.len(), 7);
}

#[test]
fn attributes_come_across_typed_and_sorted() {
    // Types come from Parquet; sorted by name like every reader.
    let dir = tempfile::tempdir().unwrap();
    let (features, _) = read_all(&fixture(dir.path(), 3));
    let names: Vec<&str> = features[1]
        .properties
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(names, vec!["name", "valuation"]);
    assert_eq!(features[1].properties[0].1, Value::Str("square 1".into()));
    assert_eq!(features[1].properties[1].1, Value::Double(1.5));
}

#[test]
fn the_geometry_survives_the_round_trip() {
    // A 0.2-degree square: five points, real area, the expected winding.
    let dir = tempfile::tempdir().unwrap();
    let (features, _) = read_all(&fixture(dir.path(), 1));
    let ring = &features[0].parts[0];
    assert_eq!(ring.len(), 5, "a square plus its closing point");
    assert_eq!(ring.first(), ring.last());

    let double_area: i64 = ring
        .windows(2)
        .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
        .sum();
    assert!(
        double_area > 0,
        "an exterior ring should be positively wound, got {double_area}"
    );
}

#[test]
fn a_parquet_file_that_is_not_geoparquet_says_so() {
    // Plain Parquet has no `geo` metadata: a clear error, not a cast failure.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Float64, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Float64Array::from(vec![1.0]))],
    )
    .unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let error = read(
        &path,
        Z,
        EXTENT,
        &Selection::default(),
        &mut |_| {},
        &mut |_| Ok(()),
    )
    .expect_err("should refuse");
    let said = format!("{error:#}");
    assert!(
        said.contains("not GeoParquet"),
        "should say what is wrong with it, got: {said}"
    );
}

/// Squares in `geometry`, the primary column by its name, and a label point
/// for each in `label`.
fn two_geometries(dir: &std::path::Path) -> std::path::PathBuf {
    use geoarrow_array::builder::PointBuilder;
    use geoarrow_schema::PointType;

    let squares: Vec<geo_types::Polygon<f64>> = (0..3)
        .map(|i| square(172.0 + f64::from(i), -44.0, 0.2))
        .collect();
    let labels: Vec<geo_types::Point<f64>> = (0..3)
        .map(|i| geo_types::Point::new(172.1 + f64::from(i), -43.9))
        .collect();
    let polygons = PolygonBuilder::from_polygons(
        &squares,
        PolygonType::new(Dimension::XY, Default::default()).with_coord_type(CoordType::Separated),
    )
    .finish();
    let points = PointBuilder::from_points(
        labels.iter(),
        PointType::new(Dimension::XY, Default::default()).with_coord_type(CoordType::Separated),
    )
    .finish();

    let schema = Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        polygons.data_type().to_field("geometry", true),
        points.data_type().to_field("label", true),
    ]);
    let names: Vec<String> = (0..3).map(|i| format!("square {i}")).collect();
    let batch = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![
            Arc::new(StringArray::from(names)),
            polygons.to_array_ref(),
            points.to_array_ref(),
        ],
    )
    .unwrap();

    let path = dir.join("two.parquet");
    let mut encoder =
        GeoParquetRecordBatchEncoder::try_new(&schema, &GeoParquetWriterOptions::default())
            .unwrap();
    let mut writer = ArrowWriter::try_new(
        std::fs::File::create(&path).unwrap(),
        encoder.target_schema(),
        None,
    )
    .unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
    path
}

fn read_field(path: &std::path::Path, field: Option<&str>) -> anyhow::Result<Vec<SourceFeature>> {
    let mut out = Vec::new();
    read(
        path,
        Z,
        EXTENT,
        &Selection {
            geometry_field: field.map(str::to_string),
            ..Selection::default()
        },
        &mut |_| {},
        &mut |feature| {
            out.push(feature);
            Ok(())
        },
    )?;
    Ok(out)
}

#[test]
fn the_primary_geometry_column_is_read_unless_another_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let path = two_geometries(dir.path());

    let primary = read_field(&path, None).unwrap();
    assert!(primary.iter().all(|f| f.kind == Kind::Rings));
    let labels = read_field(&path, Some("label")).unwrap();
    assert_eq!(labels.len(), 3);
    assert!(labels.iter().all(|f| f.kind == Kind::Points));

    // The column not read as geometry isn't read as an attribute either.
    for features in [&primary, &labels] {
        let names: Vec<&str> = features[0]
            .properties
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(names, vec!["name"]);
    }
}

#[test]
fn a_geometry_field_that_is_not_one_is_refused_with_those_there_are() {
    let dir = tempfile::tempdir().unwrap();
    let path = two_geometries(dir.path());
    for wrong in ["centroid", "name"] {
        let said = format!("{:#}", read_field(&path, Some(wrong)).unwrap_err());
        assert!(
            said.contains(wrong) && said.contains("geometry, label"),
            "{said}"
        );
    }
}

#[test]
fn asking_for_a_column_that_cannot_be_carried_is_an_error() {
    // The other geometry column isn't an MVT value: asked for by name, that
    // must be said, not silently leave the tileset without it.
    let dir = tempfile::tempdir().unwrap();
    let path = two_geometries(dir.path());
    let result = read(
        &path,
        Z,
        EXTENT,
        &Selection {
            attributes: lyv_source::Attributes::Only(vec!["name".into(), "label".into()]),
            ..Selection::default()
        },
        &mut |_| {},
        &mut |_| Ok(()),
    );
    let said = format!("{:#}", result.expect_err("label can't be carried"));
    assert!(said.contains("label"), "{said}");
}

/// One WKB column: its name, the `geometry_types` it declares, and its rows.
type DeclaredColumn<'a> = (&'a str, &'a [&'a str], Vec<Row<'a>>);

/// A row as WKT, which can say `EMPTY` where `geo_types` can't, or as
/// `geo_types`, which can hold a MultiPoint with an empty point in it (a NaN
/// point, as WKB writes one) where the WKT parser can't.
enum Row<'a> {
    Wkt(&'a str),
    Geo(geo_types::Geometry<f64>),
}

impl<'a> From<&'a str> for Row<'a> {
    fn from(text: &'a str) -> Self {
        Row::Wkt(text)
    }
}

/// A Parquet file of WKB geometry columns with a hand-written `geo` key, so
/// the declared `geometry_types` can disagree with the data, as they can in
/// files from other writers.
fn declared_wkb(dir: &std::path::Path, columns: &[DeclaredColumn]) -> std::path::PathBuf {
    use arrow_array::ArrayRef;
    use geoarrow_array::builder::WkbBuilder;
    use geoarrow_schema::WkbType;
    use parquet::file::metadata::KeyValue;

    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    let mut declared = serde_json::Map::new();
    for (name, types, rows) in columns {
        let mut builder = WkbBuilder::<i32>::new(WkbType::new(Default::default()));
        for row in rows {
            match row {
                Row::Wkt(text) => {
                    let parsed: wkt::Wkt<f64> = std::str::FromStr::from_str(text).unwrap();
                    builder.push_geometry(Some(&parsed)).unwrap();
                }
                Row::Geo(geometry) => builder.push_geometry(Some(geometry)).unwrap(),
            }
        }
        let wkb = builder.finish();
        fields.push(Field::new(*name, DataType::Binary, true));
        arrays.push(wkb.to_array_ref());
        declared.insert(
            (*name).to_string(),
            serde_json::json!({"encoding": "WKB", "geometry_types": types}),
        );
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let geo = serde_json::json!({
        "version": "1.1.0",
        "primary_column": columns[0].0,
        "columns": declared,
    });

    let path = dir.join("declared.parquet");
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.append_key_value_metadata(KeyValue::new("geo".to_string(), geo.to_string()));
    writer.close().unwrap();
    path
}

#[test]
fn a_column_declared_polygon_may_hold_multipolygons() {
    // The staging failure: a single-type reader stopped at the first row that
    // wasn't what the metadata said.
    let dir = tempfile::tempdir().unwrap();
    let path = declared_wkb(
        dir.path(),
        &[(
            "geometry",
            &["Polygon"],
            vec![
                "POLYGON((174 -41, 175 -41, 175 -40, 174 -41))".into(),
                "MULTIPOLYGON(((174 -41, 175 -41, 175 -40, 174 -41)),((176 -41, 177 -41, 177 -40, 176 -41)))".into(),
            ],
        )],
    );
    let features = read_field(&path, None).unwrap();
    assert_eq!(features.len(), 2);
    assert!(features.iter().all(|f| f.kind == Kind::Rings));
    assert_eq!(features[1].parts.len(), 2, "both polygons of the second");
}

#[test]
fn a_column_may_mix_points_lines_and_polygons_whatever_it_declares() {
    let dir = tempfile::tempdir().unwrap();
    let path = declared_wkb(
        dir.path(),
        &[(
            "geometry",
            &["Point"],
            vec![
                "POINT(174.5 -41)".into(),
                "LINESTRING(174 -41, 175 -40)".into(),
                "POLYGON((174 -41, 175 -41, 175 -40, 174 -41))".into(),
                "MULTIPOINT((174 -41), (175 -40))".into(),
            ],
        )],
    );
    let kinds: Vec<Kind> = read_field(&path, None)
        .unwrap()
        .iter()
        .map(|f| f.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![Kind::Points, Kind::Lines, Kind::Rings, Kind::Points]
    );
}

#[test]
fn an_unread_geometry_column_that_misdeclares_itself_is_left_alone() {
    // Choosing `labels` used to parse `geometry` too, and fail on its lie.
    let dir = tempfile::tempdir().unwrap();
    let path = declared_wkb(
        dir.path(),
        &[
            (
                "geometry",
                &["Polygon"],
                vec!["MULTIPOLYGON(((174 -41, 175 -41, 175 -40, 174 -41)))".into()],
            ),
            ("labels", &["Point"], vec!["POINT(174.5 -40.8)".into()]),
        ],
    );
    let features = read_field(&path, Some("labels")).unwrap();
    assert_eq!(features.len(), 1);
    assert_eq!(features[0].kind, Kind::Points);
    assert!(
        features[0].properties.is_empty(),
        "the other column isn't an attribute"
    );
}

#[test]
fn an_empty_geometry_is_dropped_as_having_none() {
    // `geo_types` has no empty point, and the conversion to it panicked. An
    // empty point in a MultiPoint goes; a feature with only empties goes too.
    let dir = tempfile::tempdir().unwrap();
    let path = declared_wkb(
        dir.path(),
        &[(
            "geometry",
            &[],
            vec![
                "POINT (174.5 -41)".into(),
                "POINT EMPTY".into(),
                Row::Geo(geo_types::Geometry::MultiPoint(
                    vec![
                        geo_types::Point::new(174.6, -41.0),
                        geo_types::Point::new(f64::NAN, f64::NAN),
                    ]
                    .into(),
                )),
                Row::Geo(geo_types::Geometry::MultiPoint(
                    vec![geo_types::Point::new(f64::NAN, f64::NAN)].into(),
                )),
                "LINESTRING EMPTY".into(),
                "POLYGON EMPTY".into(),
                "GEOMETRYCOLLECTION (POINT EMPTY)".into(),
                "GEOMETRYCOLLECTION EMPTY".into(),
            ],
        )],
    );
    let mut features = Vec::new();
    let skipped = read(
        &path,
        Z,
        EXTENT,
        &Selection::default(),
        &mut |_| {},
        &mut |feature| {
            features.push(feature);
            Ok(())
        },
    )
    .unwrap();
    // The point, and the MultiPoint without its empty point.
    assert_eq!(features.len(), 2);
    assert_eq!(features[1].parts.len(), 1);
    assert_eq!(skipped.no_geometry, 6, "{skipped:?}");
    assert_eq!(
        skipped.collapsed, 0,
        "an empty polygon is no geometry, not a small one"
    );
}
