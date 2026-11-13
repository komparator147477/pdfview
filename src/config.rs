use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::Deserialize;

use crate::printing::PrintOptions;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub theme: Theme,
    pub printing: PrintOptions,
    pub scanning: crate::scanning::Options,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub foreground: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub selected_background: Color,
    pub selected_foreground: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::Rgb(0x1e, 0x1e, 0x2e),
            surface: Color::Rgb(0x31, 0x32, 0x44),
            foreground: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x7f, 0x84, 0x9c),
            border: Color::Rgb(0x58, 0x5b, 0x70),
            accent: Color::Rgb(0xcb, 0xa6, 0xf7),
            selected_background: Color::Rgb(0xcb, 0xa6, 0xf7),
            selected_foreground: Color::Rgb(0x1e, 0x1e, 0x2e),
            success: Color::Rgb(0xa6, 0xe3, 0xa1),
            warning: Color::Rgb(0xf9, 0xe2, 0xaf),
            error: Color::Rgb(0xf3, 0x8b, 0xa8),
        }
    }
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("pdfview/config.toml")
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("parse pdfview configuration")?;
        config.printing.validate()?;
        config.scanning.validate()?;
        Ok(config)
    }

    pub fn load() -> Result<Self> {
        let path = path();
        match fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("load {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_theme_keeps_defaults() -> Result<()> {
        let config = Config::parse("[theme]\naccent = '#33ccff'\n")?;
        assert_eq!(config.theme.accent, Color::Rgb(0x33, 0xcc, 0xff));
        assert_eq!(config.theme.background, Theme::default().background);
        assert_eq!(config.printing.copies, 1);
        Ok(())
    }

    #[test]
    fn shipped_example_is_valid() -> Result<()> {
        Config::parse(include_str!("../config.example.toml"))?;
        Ok(())
    }

    #[test]
    fn config_rejects_invalid_values_and_typos() {
        assert!(Config::parse("[theme]\naccent = 'not-a-colour'").is_err());
        assert!(Config::parse("[theme]\naccnet = '#ffffff'").is_err());
        assert!(Config::parse("[printing]\ncopies = 0").is_err());
        assert!(Config::parse("[scanning]\ndpi = 0").is_err());
        assert!(Config::parse("[scanning]\ntimeout_seconds = 0").is_err());
    }
}
