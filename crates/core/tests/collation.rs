use mnemosyne_core::collation::NaturalCollator;
use serde_json::Value;
use std::cmp::Ordering;

#[test]
fn explicit_locale_sort_base_numeric_matches_reference_pair_matrix() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/collation.json")).unwrap();
    let values: Vec<_> = fixture["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let mut differences = Vec::new();
    for sample in fixture["locales"].as_array().unwrap() {
        let locale = sample["locale"].as_str().unwrap();
        let collator = NaturalCollator::new(locale).unwrap();
        let signs = sample["signs"].as_array().unwrap();
        assert_eq!(signs.len(), values.len() * values.len());
        for (i, a) in values.iter().enumerate() {
            for (j, b) in values.iter().enumerate() {
                let expected = signs[i * values.len() + j].as_i64().unwrap();
                let actual = match collator.compare(a, b) {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                };
                if expected != actual {
                    differences.push(format!(
                        "{locale} {a:?} vs {b:?}: expected {expected}, actual {actual}"
                    ));
                }
            }
        }
        let mut order: Vec<_> = (0..values.len()).collect();
        order.sort_by(|&a, &b| collator.compare(values[a], values[b]));
        let expected: Vec<_> = sample["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as usize)
            .collect();
        if order != expected {
            differences.push(format!("{locale}: stable sorted corpus differs"));
        }
    }
    assert!(
        differences.is_empty(),
        "{} differences:\n{}",
        differences.len(),
        differences
            .iter()
            .take(80)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn pinned_native_backend_matches_66_surrogate_intl_pairs() {
    assert_eq!(mnemosyne_icu78::version(), [78, 2, 0, 0]);
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/collation_surrogates.json")).unwrap();
    let sample = &fixture["collation"];
    let collator = NaturalCollator::new(sample["locale"].as_str().unwrap()).unwrap();
    let pairs = sample["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 66);
    for pair in pairs {
        let left: Vec<u16> = serde_json::from_value(pair["left"].clone()).unwrap();
        let right: Vec<u16> = serde_json::from_value(pair["right"].clone()).unwrap();
        let sign = match collator.compare_units(&left, &right) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        assert_eq!(
            sign,
            pair["sign"].as_i64().unwrap(),
            "{left:?} vs {right:?}"
        );
    }
}

#[test]
fn pinned_root_lowercase_matches_native_scalar_mapping_and_contexts() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/lowercase_native.json")).unwrap();
    assert_eq!(fixture["versions"]["icu"], "78.2");
    assert_eq!(fixture["count"], 0x110000);
    let changes: std::collections::BTreeMap<Vec<u16>, Vec<u16>> = fixture["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                serde_json::from_value(case["input"].clone()).unwrap(),
                serde_json::from_value(case["output"].clone()).unwrap(),
            )
        })
        .collect();
    assert_eq!(changes.len(), 1488);
    for point in 0u32..=0x10ffff {
        let input = if point <= 0xffff {
            vec![point as u16]
        } else {
            let point = point - 0x10000;
            vec![
                0xd800 + (point >> 10) as u16,
                0xdc00 + (point & 0x3ff) as u16,
            ]
        };
        let expected = changes.get(&input).unwrap_or(&input);
        assert_eq!(
            &mnemosyne_icu78::lowercase_units(&input).unwrap(),
            expected,
            "code point {point:x}"
        );
    }
    for case in fixture["contexts"].as_array().unwrap() {
        let input: Vec<u16> = serde_json::from_value(case["input"].clone()).unwrap();
        let expected: Vec<u16> = serde_json::from_value(case["output"].clone()).unwrap();
        assert_eq!(
            mnemosyne_icu78::lowercase_units(&input).unwrap(),
            expected,
            "{input:?}"
        );
    }
}
#[test]
fn long_native_private_use_locale_has_no_fixed_buffer_limit() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/collation_long_locale.json")).unwrap();
    let collator = NaturalCollator::new(fixture["locale"].as_str().unwrap()).unwrap();
    assert_eq!(collator.compare("file2", "file10"), Ordering::Less);
    assert_eq!(fixture["expected"], -1);
}
