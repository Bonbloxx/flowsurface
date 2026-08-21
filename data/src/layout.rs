pub use dashboard::Dashboard;
pub use pane::Pane;
use serde::{Deserialize, Serialize};

pub mod dashboard;
pub mod pane;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub name: String,
    pub dashboard: Dashboard,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            dashboard: Dashboard::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Window<T = f32> {
    pub width: T,
    pub height: T,
    pub pos_x: T,
    pub pos_y: T,
}

impl<T: Copy> Window<T> {
    pub fn size(&self) -> iced_core::Size<T> {
        iced_core::Size {
            width: self.width,
            height: self.height,
        }
    }

    pub fn position(&self) -> iced_core::Point<T> {
        iced_core::Point {
            x: self.pos_x,
            y: self.pos_y,
        }
    }
}

impl Window<f32> {
    /// Windows reports this sentinel position while a window is minimized.
    /// Persisting it makes the next process reopen beyond the desktop bounds.
    pub fn has_restorable_position(&self) -> bool {
        const MINIMIZED_SENTINEL_THRESHOLD: f32 = -30_000.0;

        self.pos_x.is_finite()
            && self.pos_y.is_finite()
            && !(self.pos_x <= MINIMIZED_SENTINEL_THRESHOLD
                && self.pos_y <= MINIMIZED_SENTINEL_THRESHOLD)
    }
}

impl Default for Window<f32> {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            pos_x: 0.0,
            pos_y: 0.0,
        }
    }
}

pub type WindowSpec = Window<f32>;

impl From<(&iced_core::Point, &iced_core::Size)> for WindowSpec {
    fn from((point, size): (&iced_core::Point, &iced_core::Size)) -> Self {
        Self {
            width: size.width,
            height: size.height,
            pos_x: point.x,
            pos_y: point.y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WindowSpec;

    #[test]
    fn rejects_windows_minimized_position_sentinel() {
        let spec = WindowSpec {
            pos_x: -32_000.0,
            pos_y: -32_000.0,
            ..WindowSpec::default()
        };

        assert!(!spec.has_restorable_position());
    }

    #[test]
    fn accepts_negative_positions_on_an_adjacent_monitor() {
        let spec = WindowSpec {
            pos_x: -1_920.0,
            pos_y: 120.0,
            ..WindowSpec::default()
        };

        assert!(spec.has_restorable_position());
    }
}
