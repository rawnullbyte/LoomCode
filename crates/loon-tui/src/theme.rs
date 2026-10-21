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
        // explicit RGB — kitty remaps 8-bit palette entries to its theme
        Style::default().fg(Color::Reset).bg(Color::Rgb(64, 64, 88))
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
/// HSV (0..1) to RGB — used by the animated gradient title.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> ratatui::style::Color {
    let i = (h * 6.0).floor() as i32 % 6;
    let f = h * 6.0 - (h * 6.0).floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ratatui::style::Color::Rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Selection highlight during an active drag: a real dim background so it is
/// unmistakably visible. Deliberate exception to the idle-rendering bg=Reset
/// invariant: a selection is transient user intent (and disappears on release).
pub fn selection_fg() -> Style {
    // explicit RGB: 8-bit palette entries (DarkGray) get remapped by kitty's theme
    Style::default().fg(Color::Reset).bg(Color::Rgb(64, 64, 88))
}
/// Menu/selected rows (autocomplete, settings): dim background bar, fg-only theme.
pub fn menu_selected() -> Style {
    // explicit RGB for the same kitty-remap reason
    Style::default().fg(Color::Reset).bg(Color::Rgb(64, 64, 88))
}
pub fn pending_fg() -> Style {
    Style::default().fg(Color::Cyan).bg(Color::Reset)
}
