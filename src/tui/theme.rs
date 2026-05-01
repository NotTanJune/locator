use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Default,
    Ocean,
    Mono,
}

impl ThemeName {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ocean => "ocean",
            Self::Mono => "mono",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Ocean,
            Self::Ocean => Self::Mono,
            Self::Mono => Self::Default,
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim() {
            "ocean" => Self::Ocean,
            "mono" => Self::Mono,
            _ => Self::Default,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: ThemeName,
    pub accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub stale: Color,
    pub text: Color,
    pub muted: Color,
    pub selected_bg: Color,
}

impl Theme {
    pub fn load() -> Self {
        theme_file()
            .and_then(|path| fs::read_to_string(path).map_err(anyhow::Error::from))
            .map(|value| Self::from_name(ThemeName::parse(&value)))
            .unwrap_or_else(|_| Self::from_name(ThemeName::Default))
    }

    pub fn from_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Default => Self {
                name,
                accent: Color::Rgb(94, 166, 150),
                ok: Color::Rgb(137, 157, 128),
                warn: Color::Rgb(201, 160, 99),
                stale: Color::Rgb(211, 116, 97),
                text: Color::Rgb(211, 222, 224),
                muted: Color::Rgb(132, 151, 158),
                selected_bg: Color::Rgb(34, 54, 58),
            },
            ThemeName::Ocean => Self {
                name,
                accent: Color::Rgb(93, 173, 226),
                ok: Color::Rgb(82, 190, 128),
                warn: Color::Rgb(245, 176, 65),
                stale: Color::Rgb(236, 112, 99),
                text: Color::Rgb(225, 232, 236),
                muted: Color::Rgb(145, 164, 174),
                selected_bg: Color::Rgb(28, 48, 68),
            },
            ThemeName::Mono => Self {
                name,
                accent: Color::Gray,
                ok: Color::White,
                warn: Color::LightYellow,
                stale: Color::LightRed,
                text: Color::White,
                muted: Color::DarkGray,
                selected_bg: Color::DarkGray,
            },
        }
    }

    pub fn cycle(self) -> Self {
        Self::from_name(self.name.next())
    }

    pub fn persist(self) -> Result<()> {
        let path = theme_file()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        fs::write(&path, self.name.label())
            .with_context(|| format!("write theme config {}", path.display()))
    }
}

fn theme_file() -> Result<PathBuf> {
    let base = dirs::config_dir().context("locate config directory")?;
    Ok(base.join("locator").join("theme"))
}
