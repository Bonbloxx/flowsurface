use flowsurface_data::{
    chart::indicator::KlineIndicator,
    layout::pane::{Pane, VisualConfig},
};

const ORIGINAL: &str = include_str!("fixtures/candle-layout-before-trade-pace.json");

fn assert_subset(original: &serde_json::Value, restored: &serde_json::Value) {
    match original {
        serde_json::Value::Object(fields) => {
            for (name, expected) in fields {
                assert_subset(expected, &restored[name]);
            }
        }
        serde_json::Value::Array(values) => {
            let restored = restored.as_array().unwrap();
            assert_eq!(values.len(), restored.len());
            for (expected, actual) in values.iter().zip(restored) {
                assert_subset(expected, actual);
            }
        }
        serde_json::Value::Number(value) if value.is_f64() => {
            // Native layout geometry is f32, while JSON numbers are parsed as f64.
            assert_eq!(
                value.as_f64().unwrap() as f32,
                restored.as_f64().unwrap() as f32
            );
        }
        _ => assert_eq!(original, restored),
    }
}

#[test]
fn original_candle_indicators_and_all_saved_settings_survive_roundtrip() {
    let pane: Pane = serde_json::from_str(ORIGINAL).unwrap();
    let Pane::KlineChart {
        indicators,
        settings,
        ..
    } = &pane
    else {
        panic!("candle pane");
    };
    assert_eq!(
        indicators,
        &[
            KlineIndicator::PreviousValueArea,
            KlineIndicator::DailyDelta,
            KlineIndicator::VisibleRangeProfile,
            KlineIndicator::OrderflowReversals,
        ]
    );
    let Some(VisualConfig::Kline(config)) = settings.visual_config else {
        panic!("visual config");
    };
    assert_eq!(config.daily_delta_ticks, 1_000);
    assert_eq!(config.daily_delta_days, 4);
    assert_eq!(config.vpvr_ticks, 1_000);
    assert_eq!(config.previous_value_area_ticks, 100);
    assert!(config.orderflow.absorption && config.orderflow.exhaustion);
    assert!(!config.orderflow.show_observed);
    assert_eq!(settings.orderflow_sources.as_ref().unwrap().len(), 3);
    let original: serde_json::Value = serde_json::from_str(ORIGINAL).unwrap();
    let restored = serde_json::to_value(&pane).unwrap();
    assert_subset(&original, &restored);
    let restarted: Pane = serde_json::from_value(restored.clone()).unwrap();
    assert_eq!(serde_json::to_value(restarted).unwrap(), restored);
}

#[test]
fn one_unknown_entry_cannot_clear_the_other_enabled_indicators() {
    let mut original: serde_json::Value = serde_json::from_str(ORIGINAL).unwrap();
    let indicators = original["KlineChart"]["indicators"].as_array_mut().unwrap();
    indicators.insert(2, serde_json::json!("IndicatorFromNewerBuild"));
    indicators.push(serde_json::Value::Null);
    let pane: Pane = serde_json::from_value(original).unwrap();
    let Pane::KlineChart { indicators, .. } = pane else {
        panic!("candle pane");
    };
    assert_eq!(
        indicators,
        [
            KlineIndicator::PreviousValueArea,
            KlineIndicator::DailyDelta,
            KlineIndicator::VisibleRangeProfile,
            KlineIndicator::OrderflowReversals,
        ]
    );
}

#[test]
fn trade_pace_coexists_with_the_original_overlays_after_restart() {
    let mut original: serde_json::Value = serde_json::from_str(ORIGINAL).unwrap();
    original["KlineChart"]["indicators"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("TradePace"));
    original["KlineChart"]["settings"]["visual_config"]["Kline"]["trade_pace"] =
        serde_json::json!({"window_secs":30,"baseline_secs":120});
    let pane: Pane = serde_json::from_value(original.clone()).unwrap();
    let restarted: Pane = serde_json::from_value(serde_json::to_value(pane).unwrap()).unwrap();
    let restored = serde_json::to_value(restarted).unwrap();
    assert_subset(&original, &restored);
    assert_eq!(
        restored["KlineChart"]["indicators"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn missing_or_corrupt_marker_snapshot_does_not_reset_the_pane() {
    let mut value: serde_json::Value = serde_json::from_str(ORIGINAL).unwrap();
    value["KlineChart"]["orderflow_state"] = serde_json::json!({"version": 999});
    let pane: Pane = serde_json::from_value(value).unwrap();
    let Pane::KlineChart {
        orderflow_state,
        indicators,
        ..
    } = pane
    else {
        panic!("candle pane")
    };
    assert!(orderflow_state.is_none());
    assert!(indicators.contains(&KlineIndicator::OrderflowReversals));
}
