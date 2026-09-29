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
