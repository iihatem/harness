//! The styles the terminal UI draws with. With `NO_COLOR` set to anything but an empty string
//! (<https://no-color.org>), no colour is used at all: text keeps only bold, dim, italic,
//! underline and reverse, and diffs keep their `+` and `-` markers.

use ratatui::style::{Color, Modifier, Style};

/// Whether and how the UI uses colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Colours are used at all.
    pub color: bool,
    /// The terminal shows 24-bit colours (`COLORTERM=truecolor` or `24bit`); otherwise syntax
    /// highlighting uses the nearest of the 256 standard colours.
    pub truecolor: bool,
}

impl Theme {
    /// The theme for this process's environment: `NO_COLOR` and `COLORTERM`.
    pub fn from_env() -> Theme {
        Theme::from_vars(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
        )
    }

    /// The theme for these values of `NO_COLOR` and `COLORTERM`.
    pub fn from_vars(no_color: Option<&str>, colorterm: Option<&str>) -> Theme {
        Theme {
            color: no_color.is_none_or(str::is_empty),
            truecolor: matches!(colorterm, Some("truecolor" | "24bit")),
        }
    }

    /// Colour and 24-bit colour.
    pub fn colored() -> Theme {
        Theme {
            color: true,
            truecolor: true,
        }
    }

    /// No colour at all, as with `NO_COLOR`.
    pub fn monochrome() -> Theme {
        Theme {
            color: false,
            truecolor: false,
        }
    }

    fn fg(&self, style: Style, color: Color) -> Style {
        if self.color { style.fg(color) } else { style }
    }

    pub fn plain(&self) -> Style {
        Style::default()
    }

    pub fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn dim(&self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    pub fn italic(&self) -> Style {
        Style::default().add_modifier(Modifier::ITALIC)
    }

    /// What the user typed.
    pub fn user(&self) -> Style {
        self.fg(self.bold(), Color::Cyan)
    }

    /// The prompt marker and other accents.
    pub fn accent(&self) -> Style {
        self.fg(self.bold(), Color::Magenta)
    }

    pub fn heading(&self) -> Style {
        self.fg(self.bold(), Color::Cyan)
    }

    /// Inline code.
    pub fn code(&self) -> Style {
        self.fg(Style::default(), Color::Yellow)
    }

    pub fn link(&self) -> Style {
        self.fg(
            Style::default().add_modifier(Modifier::UNDERLINED),
            Color::Blue,
        )
    }

    pub fn quote(&self) -> Style {
        self.fg(self.italic(), Color::Gray)
    }

    /// A diff's added lines.
    pub fn added(&self) -> Style {
        self.fg(Style::default(), Color::Green)
    }

    /// A diff's removed lines.
    pub fn removed(&self) -> Style {
        self.fg(Style::default(), Color::Red)
    }

    /// A diff's hunk headers.
    pub fn hunk(&self) -> Style {
        self.fg(self.dim(), Color::Cyan)
    }

    pub fn error(&self) -> Style {
        self.fg(self.bold(), Color::Red)
    }

    pub fn warning(&self) -> Style {
        self.fg(Style::default(), Color::Yellow)
    }

    /// Something selected in a list.
    pub fn selected(&self) -> Style {
        if self.color {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// A 24-bit colour from syntax highlighting, as this terminal can show it.
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> Option<Color> {
        if !self.color {
            None
        } else if self.truecolor {
            Some(Color::Rgb(r, g, b))
        } else {
            Some(Color::Indexed(xterm256(r, g, b)))
        }
    }
}

/// The nearest of the 256 standard terminal colours: the 6×6×6 colour cube or the 24 greys.
fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| (i32::from(**level) - i32::from(v)).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let distance = |x: [u8; 3]| -> i32 {
        [r, g, b]
            .iter()
            .zip(x)
            .map(|(a, b)| (i32::from(*a) - i32::from(b)).pow(2))
            .sum()
    };
    let cube_rgb = [
        LEVELS[ri as usize],
        LEVELS[gi as usize],
        LEVELS[bi as usize],
    ];
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let grey_index = (average.saturating_sub(3) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_index;
    if distance([grey, grey, grey]) < distance(cube_rgb) {
        232 + grey_index
    } else {
        cube
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_colours_map_to_the_cube_and_greys() {
        assert_eq!(xterm256(0, 0, 0), 16);
        assert_eq!(xterm256(255, 255, 255), 231);
        assert_eq!(xterm256(255, 0, 0), 196);
        assert_eq!(xterm256(128, 128, 128), 244);
    }
}
