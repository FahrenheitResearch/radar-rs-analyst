//! Persisted app settings. Serialization format is intentionally not fixed yet.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    pub startup_site: Option<String>,
    pub polling_interval_seconds: u64,
    pub saved_layout_slots: usize,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            startup_site: None,
            polling_interval_seconds: 60,
            saved_layout_slots: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_eight_layout_slots() {
        assert_eq!(AppSettings::default().saved_layout_slots, 8);
    }
}
