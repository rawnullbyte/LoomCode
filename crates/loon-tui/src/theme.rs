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
    fg_warn: Color,
    #[allow(dead_code)] // used by member_fg helper consumers in a later milestone
    fg_member: Color,
    /// Function/method names in highlighted code. A distinct slot rather than reusing
    /// accent: `normalize_line` maps both `Cyan` and `Magenta` to accent, so without a
    /// separate colour a keyword and a function call render identically.
    fg_fn: Color,
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
                fg_warn: Color::Yellow,
                fg_member: Color::Yellow,
                // Blue is a distinct ANSI slot, so a function name stands apart from a
                // keyword (Magenta → accent) even in a plain terminal.
                fg_fn: Color::Blue,
            },
            AppearanceMode::Opaque => Theme {
                transparent: false,
                bg: Color::Rgb(24, 24, 28),
                fg_base: Color::Rgb(220, 220, 224),
                fg_dim: Color::Rgb(120, 120, 128),
                fg_accent: Color::Rgb(102, 204, 255),
                fg_ok: Color::Rgb(126, 211, 126),
                fg_err: Color::Rgb(240, 113, 120),
                fg_warn: Color::Rgb(235, 203, 139),
                fg_member: Color::Rgb(235, 203, 139),
                fg_fn: Color::Rgb(158, 190, 226),
            },
        }
    }

    pub fn from_palette(name: &str, transparent: bool) -> Self {
        let mut t = Self::from_mode(if transparent {
            AppearanceMode::Transparent
        } else {
            AppearanceMode::Opaque
        });
        if !transparent {
            match name {
                "light" => {
                    t.bg = Color::Rgb(247, 248, 250);
                    t.fg_base = Color::Rgb(30, 40, 56);
                    t.fg_dim = Color::Rgb(83, 96, 115);
                    t.fg_accent = Color::Rgb(27, 89, 181);
                    t.fg_ok = Color::Rgb(25, 117, 66);
                    t.fg_err = Color::Rgb(184, 38, 56);
                    t.fg_warn = Color::Rgb(146, 101, 0);
                    t.fg_fn = Color::Rgb(124, 58, 150);
                }
                "violet" => {
                    t.bg = Color::Rgb(25, 23, 37);
                    t.fg_base = Color::Rgb(229, 224, 242);
                    t.fg_dim = Color::Rgb(151, 142, 171);
                    t.fg_accent = Color::Rgb(188, 155, 246);
                    t.fg_ok = Color::Rgb(149, 213, 178);
                    t.fg_fn = Color::Rgb(126, 200, 227);
                }
                "ember" => {
                    t.bg = Color::Rgb(32, 25, 22);
                    t.fg_base = Color::Rgb(237, 225, 210);
                    t.fg_dim = Color::Rgb(161, 145, 127);
                    t.fg_accent = Color::Rgb(240, 176, 109);
                    t.fg_ok = Color::Rgb(162, 204, 151);
                    t.fg_fn = Color::Rgb(200, 160, 200);
                }
                "graphite" => {
                    t.bg = Color::Rgb(20, 22, 25);
                    t.fg_base = Color::Rgb(222, 228, 235);
                    t.fg_dim = Color::Rgb(139, 151, 167);
                    t.fg_accent = Color::Rgb(158, 190, 226);
                    t.fg_fn = Color::Rgb(190, 160, 220);
                }
                "ocean" => {
                    t.bg = Color::Rgb(14, 28, 40);
                    t.fg_base = Color::Rgb(214, 231, 239);
                    t.fg_accent = Color::Rgb(91, 218, 200);
                    t.fg_warn = Color::Rgb(240, 200, 120);
                    t.fg_fn = Color::Rgb(140, 170, 240);
                }
                _ => {}
            }
        }
        t
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
    pub fn warn(&self) -> Style {
        self.bg(Style::default().fg(self.fg_warn))
    }
    /// Function/method names in highlighted code.
    pub fn func(&self) -> Style {
        self.bg(Style::default().fg(self.fg_fn))
    }
    /// Type/trait names: accent, italic, so it reads apart from a keyword without a new
    /// colour. Attributes (bold) and comments (italic) use the same trick.
    pub fn type_style(&self) -> Style {
        self.bg(Style::default()
            .fg(self.fg_accent)
            .add_modifier(Modifier::ITALIC))
    }
    /// `#[derive]`, `@decorator`: warn, bold.
    pub fn attribute(&self) -> Style {
        self.bg(Style::default()
            .fg(self.fg_warn)
            .add_modifier(Modifier::BOLD))
    }
    /// Operators and punctuation: dim, so structure is visible without competing with
    /// keywords.
    pub fn punctuation(&self) -> Style {
        self.bg(Style::default().fg(self.fg_dim))
    }
    /// Permission-mode colour: bypass red, auto yellow, ask green, allowlist accent.
    pub fn mode_style(&self, mode: &str) -> Style {
        match mode {
            "bypass" => self.err(),
            "auto" => self.warn(),
            "ask" => self.ok(),
            "allowlist" => self.accent(),
            _ => self.dim(),
        }
    }
    pub fn border(&self) -> Style {
        self.bg(Style::default().fg(self.fg_dim))
    }
    /// Menu/selected rows (autocomplete, settings): dim background bar.
    pub fn menu_selected(&self) -> Style {
        // explicit RGB — kitty remaps 8-bit palette entries to its theme
        if self.transparent {
            self.accent_bold().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(self.fg_base).bg(match self.bg {
                Color::Rgb(247, 248, 250) => Color::Rgb(221, 230, 244),
                _ => Color::Rgb(46, 57, 76),
            })
        }
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
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
        .bg(Color::Reset)
}
/// Default foreground for plain banner/transcript text.
pub fn base_style() -> Style {
    Style::default().fg(Color::Reset).bg(Color::Reset)
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
