//! Small deterministic motion primitives; no random layout changes or fake progress.
use ratatui::style::Color;
pub fn spinner(tick: u64) -> char {
    ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][(tick as usize / 2) % 10]
}
pub fn pulse(tick: u64) -> Color {
    let phase = (tick % 60) as f32 / 60.0 * std::f32::consts::TAU;
    let k = (phase.sin() + 1.0) * 0.5;
    Color::Rgb(
        (92.0 + 56.0 * k) as u8,
        (156.0 + 64.0 * k) as u8,
        (205.0 + 40.0 * k) as u8,
    )
}
/// The Loom wordmark gradient, sampled for one glyph: a hue that drifts along the word
/// and over time, with a slow wave so the colour band travels rather than scrolling
/// linearly. This is the banner's own sweep (see `banner::wordmark_row`), reused so the
/// effort menu's `ultra` row shimmers exactly like the logo.
pub fn logo_gradient(tick: u64, col: usize, row: usize) -> Color {
    let wave = ((tick as f32) * 0.05 + col as f32 * 0.45).sin() * 0.02;
    let hue =
        (0.47 + col as f32 * 0.0075 + (tick as f32) * 0.0009 + row as f32 * 0.01 + wave) % 1.0;
    crate::theme::hsv_to_rgb(hue, 0.55, 1.0)
}
pub fn scan(tick: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let period = (width * 2).saturating_sub(2).max(1);
    let phase = (tick as usize / 2) % period;
    let pos = if phase < width { phase } else { period - phase };
    (0..width)
        .map(|i| {
            if i == pos {
                '━'
            } else if i.abs_diff(pos) == 1 {
                '─'
            } else {
                '·'
            }
        })
        .collect()
}
