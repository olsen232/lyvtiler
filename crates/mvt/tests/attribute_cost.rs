//! What a feature's attributes cost in an encoded tile, checked against the
//! encoder itself rather than a formula.

use std::collections::BTreeMap;

use geo_types::Coord;
use lyv_core::{Chain, Value};
use lyv_mvt::{attribute_cost, encode_chains_with};

type Properties = Vec<(String, Value)>;

fn text(key: &str, value: &str) -> (String, Value) {
    (key.to_string(), Value::Str(value.to_string()))
}

/// The bytes attributes add, beyond the same tile with none.
fn measured(features: &[Properties]) -> usize {
    let chains: Vec<Chain> = (0..features.len() as u64)
        .map(|feature| Chain {
            feature,
            points: vec![Coord { x: 1, y: 2 }],
        })
        .collect();
    let attributes: BTreeMap<u64, Properties> = features
        .iter()
        .cloned()
        .zip(0..)
        .map(|(p, id)| (id, p))
        .collect();
    encode_chains_with("l", 4096, &chains, &attributes).len()
        - encode_chains_with("l", 4096, &chains, &BTreeMap::new()).len()
}

fn estimated(features: &[Properties]) -> usize {
    features.iter().map(|p| attribute_cost(p)).sum()
}

/// How far the estimate is from the truth, as a factor.
fn ratio(features: &[Properties]) -> f64 {
    estimated(features) as f64 / measured(features).max(1) as f64
}

#[test]
fn a_feature_with_no_attributes_costs_nothing() {
    assert_eq!(attribute_cost(&[]), 0);
}

#[test]
fn the_estimate_is_close_when_every_value_is_distinct() {
    // What the estimate is for: every value distinct, so nothing pools.
    let features: Vec<Properties> = (0..200)
        .map(|i| {
            vec![
                text("address", &format!("{i} Someplace Road, Suburbia")),
                text("title", &format!("NA{i}A/{i}")),
                ("area".to_string(), Value::Double(f64::from(i) * 1.5)),
                ("valuation".to_string(), Value::Int(i64::from(i) * 1000)),
            ]
        })
        .collect();

    let ratio = ratio(&features);
    assert!(
        (0.9..=1.1).contains(&ratio),
        "estimate {} against measured {}, a factor of {ratio:.2}",
        estimated(&features),
        measured(&features)
    );
}

#[test]
fn the_estimate_never_under_reads_a_pooled_layer() {
    // One value shared by all: MVT writes it once, but the estimate can't know
    // that without the whole tile, so it over-reads — the safe direction.
    let features: Vec<Properties> = (0..200)
        .map(|_| vec![text("kind", "residential")])
        .collect();

    assert!(
        estimated(&features) >= measured(&features),
        "estimate {} under-read the real cost {}",
        estimated(&features),
        measured(&features)
    );
}

#[test]
fn a_longer_string_costs_more_than_a_shorter_one() {
    let short = vec![text("name", "a")];
    let long = vec![text("name", &"a".repeat(500))];
    assert!(attribute_cost(&long) > attribute_cost(&short) + 400);
}

#[test]
fn more_attributes_cost_more_than_fewer() {
    let one = vec![("a".to_string(), Value::Int(1))];
    let four = vec![
        ("a".to_string(), Value::Int(1)),
        ("b".to_string(), Value::Int(2)),
        ("c".to_string(), Value::Int(3)),
        ("d".to_string(), Value::Int(4)),
    ];
    assert!(attribute_cost(&four) > attribute_cost(&one));
}

#[test]
fn every_value_type_is_costed() {
    // Completeness: a type costed at zero would vanish from the estimate.
    for value in [
        Value::Str("hello".into()),
        Value::Float(1.5),
        Value::Double(1.5),
        Value::Int(-7),
        Value::Uint(7),
        Value::Sint(-7),
        Value::Bool(true),
    ] {
        let cost = attribute_cost(&[("k".to_string(), value.clone())]);
        assert!(cost > 0, "{value:?} was costed at {cost}");
    }
}
