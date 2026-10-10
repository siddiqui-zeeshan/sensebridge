//! What the user chose, kept in a JSON file in the app's config directory.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Response {
    Fast,
    Balanced,
    Smooth,
}

impl Response {
    /// The controller's haptics queue depth. A deeper queue rides out Bluetooth
    /// hiccups at the cost of latency; 16 is the lowest that plays cleanly.
    pub fn buffer(self) -> u8 {
        match self {
            Self::Fast => 16,
            Self::Balanced => 32,
            Self::Smooth => 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Haptics strength; 1.0 sends what the game sent.
    pub strength: f32,
    pub response: Response,
    /// Start bridging as soon as the app opens.
    pub start_on_launch: bool,
    /// Closing the window keeps the app running in the tray.
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { strength: 1.0, response: Response::Fast, start_on_launch: true, close_to_tray: true }
    }
}

impl Settings {
    pub const MAX_STRENGTH: f32 = 2.0;

    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map(Self::sanitised)
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self).expect("settings serialise"))
    }

    pub fn sanitised(mut self) -> Self {
        self.strength = if self.strength.is_finite() {
            self.strength.clamp(0.0, Self::MAX_STRENGTH)
        } else {
            1.0
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("sensebridge-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trips_and_fills_in_missing_fields() {
        let dir = std::env::temp_dir().join(format!("sensebridge-test2-{}", std::process::id()));
        let path = dir.join("settings.json");
        let settings = Settings { strength: 1.4, response: Response::Smooth, ..Default::default() };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        fs::write(&path, r#"{"strength": 0.5}"#).unwrap();
        assert_eq!(Settings::load(&path), Settings { strength: 0.5, ..Default::default() });
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strength_is_kept_in_range() {
        let wild = Settings { strength: 99.0, ..Default::default() }.sanitised();
        assert_eq!(wild.strength, Settings::MAX_STRENGTH);
        let nan = Settings { strength: f32::NAN, ..Default::default() }.sanitised();
        assert_eq!(nan.strength, 1.0);
    }

    #[test]
    fn response_maps_to_queue_depth() {
        assert_eq!(Response::Fast.buffer(), 16);
        assert!(Response::Balanced.buffer() < Response::Smooth.buffer());
    }
}
