use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Default,
    Catppuccin,
    TokyoNight,
    Gruvbox,
    Nord,
    Ocean,
    Mono,
}

impl ThemeName {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Catppuccin => "catppuccin",
            Self::TokyoNight => "tokyonight",
            Self::Gruvbox => "gruvbox",
            Self::Nord => "nord",
            Self::Ocean => "ocean",
            Self::Mono => "mono",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Catppuccin,
            Self::Catppuccin => Self::TokyoNight,
            Self::TokyoNight => Self::Gruvbox,
            Self::Gruvbox => Self::Nord,
            Self::Nord => Self::Ocean,
            Self::Ocean => Self::Mono,
            Self::Mono => Self::Default,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "catppuccin" => Self::Catppuccin,
            "tokyonight" => Self::TokyoNight,
            "gruvbox" => Self::Gruvbox,
            "nord" => Self::Nord,
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
    /// Load the in-session theme file if present (set by the `t` key), otherwise
    /// fall back to the configured default theme name.
    pub fn load_with_default(default_name: &str) -> Self {
        theme_file()
            .and_then(|path| fs::read_to_string(path).map_err(anyhow::Error::from))
            .map(|value| Self::from_name(ThemeName::parse(&value)))
            .unwrap_or_else(|_| Self::from_name(ThemeName::parse(default_name)))
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
            ThemeName::Catppuccin => Self {
                name,
                accent: Color::Rgb(203, 166, 247),
                ok: Color::Rgb(166, 227, 161),
                warn: Color::Rgb(250, 179, 135),
                stale: Color::Rgb(243, 139, 168),
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(147, 153, 178),
                selected_bg: Color::Rgb(49, 50, 68),
            },
            ThemeName::TokyoNight => Self {
                name,
                accent: Color::Rgb(122, 162, 247),
                ok: Color::Rgb(158, 206, 106),
                warn: Color::Rgb(224, 175, 104),
                stale: Color::Rgb(247, 118, 142),
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(108, 120, 162),
                selected_bg: Color::Rgb(40, 52, 87),
            },
            ThemeName::Gruvbox => Self {
                name,
                accent: Color::Rgb(142, 192, 124),
                ok: Color::Rgb(184, 187, 38),
                warn: Color::Rgb(250, 189, 47),
                stale: Color::Rgb(251, 73, 52),
                text: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
                selected_bg: Color::Rgb(60, 56, 54),
            },
            ThemeName::Nord => Self {
                name,
                accent: Color::Rgb(136, 192, 208),
                ok: Color::Rgb(163, 190, 140),
                warn: Color::Rgb(235, 203, 139),
                stale: Color::Rgb(191, 97, 106),
                text: Color::Rgb(236, 239, 244),
                muted: Color::Rgb(122, 134, 160),
                selected_bg: Color::Rgb(59, 66, 82),
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
