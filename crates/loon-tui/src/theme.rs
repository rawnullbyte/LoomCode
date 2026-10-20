//! The only place UI styles are created.
//!
//! Transparent mode: **every style has `bg = Color::Reset`** so the terminal's own
//! background (and compositor blur) shows through. Selection/highlight is foreground +
//! underline; reverse video is never used. Opaque mode (explicit opt-in only) paints a
//! real background from its palette.

use loon_config::AppearanceMode;
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// True ⇒ all backgrounds are `Color::Reset`.
    pub transparent: bool,
    bg: Color,
    fg_base: Color,
    fg_dim: Color,
    fg_accent: Color,
    fg_ok: Color,
    fg_err: Color,
    #[allow(dead_code)] // used by member_fg helper consumers in a later milestone
    fg_member: Color,
}

impl Theme {
    pub fn from_mode(mode: AppearanceMode) -> Theme {
        match mode {
            AppearanceMode::Transparent => Theme {
                transparent: true,
                bg: Color::Reset,
                fg_base: Color::Reset,
                fg_dim: Color::DarkGray,
                fg_accent: Color::Cyan,
                fg_ok: Color::Green,
                fg_err: Color::Red,
                fg_member: Color::Yellow,
            },
            AppearanceMode::Opaque => Theme {
                transparent: false,
                bg: Color::Rgb(24, 24, 28),
                fg_base: Color::Rgb(220, 220, 224),
                fg_dim: Color::Rgb(120, 120, 128),
                fg_accent: Color::Rgb(102, 204, 255),
                fg_ok: Color::Rgb(126, 211, 126),
                fg_err: Color::Rgb(240, 113, 120),
                fg_member: Color::Rgb(235, 203, 139),
            },
        }
    }

    pub fn base(&self) -> Style {
        self.bg(Style::default().fg(self.fg_base))
    }
    pub fn dim(&self) -> Style {
        self.bg(Style::default().fg(self.fg_dim))
    }
    pub fn accent(&self) -> Style {
        self.bg(Style::default().fg(self.fg_accent))
    }
    pub fn accent_bold(&self) -> Style {
        self.bg(Style::default()
            .fg(self.fg_accent)
            .add_modifier(Modifier::BOLD))
    }
    pub fn ok(&self) -> Style {
        self.bg(Style::default().fg(self.fg_ok))
    }
    pub fn err(&self) -> Style {
        self.bg(Style::default().fg(self.fg_err))
    }
    pub fn border(&self) -> Style {
        self.bg(Style::default().fg(self.fg_dim))
    }
    /// Menu/selected rows (autocomplete, settings): dim background bar.
    pub fn menu_selected(&self) -> Style {
        // DarkGray bg is the deliberate highlight; kept in both modes
        Style::default()
            .fg(self.fg_accent)
            .add_modifier(Modifier::BOLD)
            .bg(Color::DarkGray)
    }

    fn bg(&self, mut s: Style) -> Style {
        if self.transparent {
            // The invariant: application-owned cells never paint a background.
            s = s.bg(Color::Reset);
        } else {
            s = s.bg(self.bg);
        }
        s
    }
}

// Free helpers used by the transcript builder (default transparent theme).
pub fn user_fg() -> Style {
    Style::default()
        .fg(Color::Reset)
        .add_modifier(Modifier::BOLD)
        .bg(Color::Reset)
}
pub fn assistant_fg() -> Style {
    Style::default().fg(Color::Reset).bg(Color::Reset)
}
pub fn tool_ok_fg() -> Style {
    Style::default().fg(Color::Green).bg(Color::Reset)
}
pub fn tool_err_fg() -> Style {
    Style::default().fg(Color::Red).bg(Color::Reset)
}
pub fn system_fg() -> Style {
    Style::default().fg(Color::DarkGray).bg(Color::Reset)
}
pub fn member_fg() -> Style {
    Style::default().fg(Color::Yellow).bg(Color::Reset)
}
pub fn accent_fg() -> Style {
    Style::default().fg(Color::Cyan).bg(Color::Reset)
}
/// Selection highlight during an active drag: a real dim background so it is
/// unmistakably visible. Deliberate exception to the idle-rendering bg=Reset
/// invariant: a selection is transient user intent (and disappears on release).
pub fn selection_fg() -> Style {
    Style::default().fg(Color::White).bg(Color::Rgb(58, 58, 92))
}
/// Menu/selected rows (autocomplete, settings): dim background bar, fg-only theme.
pub fn menu_selected() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(ratatui::style::Modifier::BOLD)
        .bg(Color::DarkGray)
}
pub fn pending_fg() -> Style {
    Style::default().fg(Color::Cyan).bg(Color::Reset)
}
