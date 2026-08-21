use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub const SOFT_CHIME: &[u8] = include_bytes!("../assets/sounds/soft-chime.wav");
pub const BRIGHT_BELLS: &[u8] = include_bytes!("../assets/sounds/bright-bells.wav");
pub const GENTLE_ALERT: &[u8] = include_bytes!("../assets/sounds/gentle-alert.wav");

pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "blocked.svg" => Some(include_bytes!("../assets/blocked.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if path.is_empty() {
            vec!["blocked.svg".into()]
        } else {
            Vec::new()
        })
    }
}

pub fn builtin_audio(name: &str) -> Option<&'static [u8]> {
    match name {
        "builtin:soft-chime" => Some(SOFT_CHIME),
        "builtin:bright-bells" => Some(BRIGHT_BELLS),
        "builtin:gentle-alert" => Some(GENTLE_ALERT),
        _ => None,
    }
}
