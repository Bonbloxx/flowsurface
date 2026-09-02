use exchange::SerTicker;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RectangleAnnotation {
    pub start_interval: u64,
    pub end_interval: u64,
    pub start_percent: f32,
    pub end_percent: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub colors: Vec<(SerTicker, iced_core::Color)>,
    pub names: Vec<(SerTicker, String)>,
    #[serde(default)]
    pub rectangles: Vec<RectangleAnnotation>,
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn legacy_comparison_config_defaults_to_no_rectangles() {
        let config: Config = serde_json::from_str(r#"{"colors":[],"names":[]}"#)
            .expect("legacy comparison config should deserialize");

        assert!(config.rectangles.is_empty());
    }
}
