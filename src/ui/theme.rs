//! The palette every screen paints with: a set of named tokens, the presets
//! that fill them, and the one this run has chosen.
//!
//! `terminal` is the default and uses the sixteen ANSI colours, so whatever
//! palette the terminal itself is set to shows through — over SSH too.
//! `terminal-light` swaps the few that fail on a white ground. `mono` is what
//! `NO_COLOR` selects: every colour reset, so weight and glyphs carry each
//! distinction alone. `custom` is built from the palette in `config.toml`,
//! which is what the `theme` tool writes there in its own vocabulary.

#[cfg(not(test))]
use std::sync::{OnceLock, PoisonError, RwLock};

use anyhow::{Result, bail};
use ratatui::style::Color;
use ratatui::widgets::BorderType;

use crate::config::{Config, Palette};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub text: Color,
    pub body: Color,
    pub link: Color,
    /// Headings that sit between the pane title and the body: the table
    /// header, the section rules in the details pane.
    pub header: Color,
    /// The frame of a pane nothing is focused on, and of every overlay.
    pub border: Color,
    pub border_focused: Color,
    pub selected_background: Color,
    /// The text of a selected row where the palette says so; `Reset` keeps
    /// whatever the cell was painted in.
    pub selection_fg: Color,
    /// A dimmer wash than `selected_background`, laid under a hovered row so
    /// its colour-coded cells keep their own foregrounds.
    pub hover_background: Color,
    pub info: Color,
    pub success: Color,
    /// What an expiry inside thirty days paints in. It is deliberately not
    /// `error`: a secret that is about to lapse is a fact about the clock,
    /// not a failure.
    pub warning: Color,
    pub error: Color,
    pub scrollbar: Color,
    pub search_match: Color,
    /// Restrained badge colours a tag is hashed into, so one tag always reads
    /// the same wherever it appears.
    pub tag_palette: [Color; 6],
    pub border_type: BorderType,
    /// Whether the screen behind a modal is washed out while it is open.
    pub dim_behind_modals: bool,
}

impl Theme {
    /// The sixteen ANSI colours, as the terminal has them set.
    #[must_use]
    pub const fn terminal() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGray,
            text: Color::White,
            body: Color::Gray,
            link: Color::Blue,
            header: Color::Cyan,
            border: Color::DarkGray,
            border_focused: Color::Cyan,
            selected_background: Color::DarkGray,
            selection_fg: Color::Reset,
            hover_background: Color::Indexed(237),
            info: Color::Yellow,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            scrollbar: Color::DarkGray,
            search_match: Color::Yellow,
            tag_palette: [
                Color::Cyan,
                Color::Blue,
                Color::Magenta,
                Color::Green,
                Color::Yellow,
                Color::White,
            ],
            border_type: BorderType::Rounded,
            dim_behind_modals: true,
        }
    }

    /// The ANSI palette again, with the colours that vanish on a white ground
    /// — white text, yellow, cyan, a near-black hover — swapped for ones that
    /// do not. Yellow's jobs go to a dark orange from the 256-colour cube.
    #[must_use]
    pub const fn terminal_light() -> Self {
        const AMBER: Color = Color::Indexed(130);
        Self {
            accent: Color::Blue,
            muted: Color::DarkGray,
            text: Color::Black,
            body: Color::Reset,
            link: Color::Blue,
            header: Color::Blue,
            border: Color::Gray,
            border_focused: Color::Blue,
            selected_background: Color::Indexed(253),
            selection_fg: Color::Reset,
            hover_background: Color::Indexed(255),
            info: Color::Blue,
            success: Color::Green,
            warning: AMBER,
            error: Color::Red,
            scrollbar: Color::Gray,
            search_match: AMBER,
            tag_palette: [
                Color::Indexed(30),
                Color::Blue,
                Color::Magenta,
                Color::Green,
                AMBER,
                // Not `DarkGray`: that is this palette's muted colour, and a
                // tag hashed onto it would read as a disabled one.
                Color::Indexed(52),
            ],
            border_type: BorderType::Rounded,
            dim_behind_modals: true,
        }
    }

    /// No colour at all: what `NO_COLOR` asks for. Weight and glyphs carry
    /// every distinction, the corners stay plain, and nothing is dimmed.
    #[must_use]
    pub const fn mono() -> Self {
        Self {
            accent: Color::Reset,
            muted: Color::Reset,
            text: Color::Reset,
            body: Color::Reset,
            link: Color::Reset,
            header: Color::Reset,
            border: Color::Reset,
            border_focused: Color::Reset,
            selected_background: Color::Reset,
            selection_fg: Color::Reset,
            hover_background: Color::Reset,
            info: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            scrollbar: Color::Reset,
            search_match: Color::Reset,
            tag_palette: [Color::Reset; 6],
            border_type: BorderType::Plain,
            dim_behind_modals: false,
        }
    }

    /// A palette in the `theme` tool's vocabulary, mapped onto these tokens.
    ///
    /// The terminal's own background is left alone: the tool has already set
    /// the terminal to `bg`, and painting it again would only fight a
    /// translucent window. The hover wash is a shade the palette does not
    /// name, mixed between the ground and the overlay so a hovered row is
    /// visibly lighter than the ground and visibly dimmer than a selected
    /// one.
    #[must_use]
    pub fn from_palette(palette: &Palette) -> Self {
        let hover = palette.bg.mix(palette.overlay, 0.5);
        Self {
            accent: palette.accent.into(),
            muted: palette.muted.into(),
            text: palette.fg.into(),
            body: palette.subtle.into(),
            link: palette.blue.into(),
            header: palette.subtle.into(),
            border: palette.overlay.into(),
            border_focused: palette.accent.into(),
            selected_background: palette.overlay.into(),
            selection_fg: palette.fg.into(),
            hover_background: hover.into(),
            info: palette.yellow.into(),
            success: palette.green.into(),
            warning: palette.orange.into(),
            error: palette.red.into(),
            scrollbar: palette.overlay.into(),
            search_match: palette.yellow.into(),
            tag_palette: [
                palette.cyan.into(),
                palette.blue.into(),
                palette.accent.into(),
                palette.green.into(),
                palette.teal.into(),
                palette.orange.into(),
            ],
            border_type: BorderType::Rounded,
            dim_behind_modals: true,
        }
    }

    /// The theme the environment asks for before any file is read: `mono`
    /// under `NO_COLOR`, otherwise whichever preset `AZ_TUI_THEME` names, and
    /// the terminal's own colours when it names none.
    ///
    /// A run resolves its theme properly once `config.toml` has been read;
    /// this is what the first frame paints with, and what a test run paints
    /// with from end to end. `custom` needs the file, so it lands here as
    /// `terminal`.
    #[must_use]
    pub fn from_env() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::mono();
        }
        match std::env::var("AZ_TUI_THEME").ok().as_deref().map(str::trim) {
            Some("terminal-light" | "terminal_light" | "light") => Self::terminal_light(),
            Some("mono" | "monochrome") => Self::mono(),
            _ => Self::terminal(),
        }
    }
}

/// Which theme to paint with, as the flag, the variable and the file name it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeChoice {
    Terminal,
    TerminalLight,
    Mono,
    Custom,
}

impl ThemeChoice {
    pub const NAMES: [&'static str; 4] = ["terminal", "terminal-light", "mono", "custom"];

    /// Reads a theme name; anything else is an error naming the four.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "terminal" => Ok(Self::Terminal),
            "terminal-light" | "terminal_light" | "light" => Ok(Self::TerminalLight),
            "mono" | "monochrome" => Ok(Self::Mono),
            "custom" => Ok(Self::Custom),
            other => bail!(
                "unknown theme {other:?}; the themes are {}",
                Self::NAMES.join(", ")
            ),
        }
    }

    /// Which theme this run paints with: `NO_COLOR` first, then whatever
    /// `--theme` or `AZ_TUI_THEME` settled on, then the file's `preset`,
    /// then the custom palette if the file carries one, and the terminal's
    /// own colours when nothing said otherwise.
    pub fn resolve(no_color: bool, chosen: Option<Self>, config: &Config) -> Result<Self> {
        if no_color {
            return Ok(Self::Mono);
        }
        if let Some(choice) = chosen {
            return Ok(choice);
        }
        if let Some(preset) = config.theme.preset.as_deref() {
            return Self::parse(preset);
        }
        Ok(if config.theme.custom.is_some() {
            Self::Custom
        } else {
            Self::Terminal
        })
    }

    /// The theme itself.
    pub fn theme(self, config: &Config) -> Result<Theme> {
        Ok(match self {
            Self::Terminal => Theme::terminal(),
            Self::TerminalLight => Theme::terminal_light(),
            Self::Mono => Theme::mono(),
            Self::Custom => {
                let Some(palette) = config.theme.custom.as_ref() else {
                    bail!("theme \"custom\" needs a [theme.custom] palette in config.toml");
                };
                Theme::from_palette(palette)
            }
        })
    }
}

/// `--theme` before `AZ_TUI_THEME`; a name neither of the four is a startup
/// error, so a typo is said out loud rather than silently ignored.
pub fn chosen_theme(flag: Option<&str>, env: Option<&str>) -> Result<Option<ThemeChoice>> {
    let raw = flag.or(env).map(str::trim).filter(|raw| !raw.is_empty());
    raw.map(ThemeChoice::parse).transpose()
}

#[cfg(not(test))]
fn store() -> &'static RwLock<Theme> {
    static THEME: OnceLock<RwLock<Theme>> = OnceLock::new();
    THEME.get_or_init(|| RwLock::new(Theme::from_env()))
}

/// The theme every screen paints with this frame.
#[cfg(not(test))]
#[must_use]
pub fn theme() -> Theme {
    *store().read().unwrap_or_else(PoisonError::into_inner)
}

/// Repaints from the next frame on in `theme`.
#[cfg(not(test))]
pub fn set_theme(theme: Theme) {
    *store().write().unwrap_or_else(PoisonError::into_inner) = theme;
}

// Under test each thread paints with its own theme, so a test that switches
// themes cannot change what a renderer test on another thread is comparing
// against.
#[cfg(test)]
thread_local! {
    static THEME: std::cell::Cell<Theme> = std::cell::Cell::new(Theme::from_env());
}

#[cfg(test)]
#[must_use]
pub fn theme() -> Theme {
    THEME.with(std::cell::Cell::get)
}

#[cfg(test)]
pub fn set_theme(theme: Theme) {
    THEME.with(|cell| cell.set(theme));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn custom_config() -> Config {
        config::parse(
            r##"
[theme.custom]
name = "grok-night"
bg = "#141414"
bg_deep = "#0a0a0a"
surface = "#1c1c1c"
overlay = "#242424"
fg = "#e1e1e1"
subtle = "#c8c8c8"
muted = "#6c6c6c"
accent = "#bb9af7"
red = "#f7768e"
green = "#9ece6a"
yellow = "#e0af68"
blue = "#7aa2f7"
cyan = "#7dcfff"
orange = "#ff9e64"
teal = "#1abc9c"
"##,
        )
        .unwrap()
    }

    #[test]
    fn no_color_wins_over_everything() {
        let config = custom_config();
        assert_eq!(
            ThemeChoice::resolve(true, Some(ThemeChoice::Terminal), &config).unwrap(),
            ThemeChoice::Mono
        );
    }

    #[test]
    fn the_flag_beats_the_file_and_the_file_beats_the_palette() {
        let mut config = custom_config();
        assert_eq!(
            ThemeChoice::resolve(false, None, &config).unwrap(),
            ThemeChoice::Custom,
            "a palette in the file is used without being asked for"
        );
        assert_eq!(
            ThemeChoice::resolve(false, Some(ThemeChoice::TerminalLight), &config).unwrap(),
            ThemeChoice::TerminalLight
        );
        config.theme.preset = Some("mono".into());
        assert_eq!(
            ThemeChoice::resolve(false, None, &config).unwrap(),
            ThemeChoice::Mono
        );
        assert_eq!(
            ThemeChoice::resolve(false, None, &Config::default()).unwrap(),
            ThemeChoice::Terminal
        );
    }

    #[test]
    fn the_flag_comes_before_the_variable_and_a_bad_name_is_an_error() {
        assert_eq!(chosen_theme(None, None).unwrap(), None);
        assert_eq!(chosen_theme(None, Some("  ")).unwrap(), None);
        assert_eq!(
            chosen_theme(Some("mono"), Some("terminal")).unwrap(),
            Some(ThemeChoice::Mono)
        );
        assert_eq!(
            chosen_theme(None, Some("terminal-light")).unwrap(),
            Some(ThemeChoice::TerminalLight)
        );
        let error = chosen_theme(None, Some("dracula")).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown theme \"dracula\"; the themes are terminal"),
            "{error:#}"
        );
    }

    #[test]
    fn custom_without_a_palette_says_what_is_missing() {
        let error = ThemeChoice::Custom.theme(&Config::default()).unwrap_err();
        assert!(format!("{error:#}").contains("[theme.custom]"), "{error:#}");
    }

    #[test]
    fn a_palette_maps_onto_the_tokens() {
        let config = custom_config();
        let theme = ThemeChoice::Custom.theme(&config).unwrap();
        assert_eq!(theme.accent, Color::Rgb(0xbb, 0x9a, 0xf7));
        assert_eq!(theme.border_focused, theme.accent);
        assert_eq!(theme.text, Color::Rgb(0xe1, 0xe1, 0xe1));
        assert_eq!(theme.error, Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(
            theme.hover_background,
            Color::Rgb(0x1c, 0x1c, 0x1c),
            "hover sits halfway between the ground and the overlay"
        );
        assert_eq!(theme.border_type, BorderType::Rounded);
        assert!(theme.dim_behind_modals);
    }

    #[test]
    fn every_coloured_preset_keeps_a_warning_apart_from_a_failure() {
        for theme in [Theme::terminal(), Theme::terminal_light()] {
            assert_ne!(theme.warning, theme.error);
            assert_ne!(theme.warning, theme.muted);
            assert_ne!(theme.text, theme.hover_background);
            assert_ne!(theme.text, theme.selected_background);
        }
        let mono = Theme::mono();
        assert_eq!(mono.accent, Color::Reset);
        assert_eq!(mono.border_type, BorderType::Plain);
        assert!(!mono.dim_behind_modals);
    }

    #[test]
    fn setting_the_theme_changes_what_is_painted_next() {
        let before = theme();
        set_theme(Theme::terminal_light());
        assert_eq!(theme(), Theme::terminal_light());
        set_theme(before);
        assert_eq!(theme(), before);
    }
}
