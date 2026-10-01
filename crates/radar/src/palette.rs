use gtk::gdk::RGBA;
use omarchy_theme::{Palette, Rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Blue,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
}

pub const CYCLE: [Hue; 6] = [
    Hue::Red,
    Hue::Orange,
    Hue::Yellow,
    Hue::Green,
    Hue::Blue,
    Hue::Purple,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorRole {
    Accent,
    Named(Hue),
    Foreground,
}

/// GNOME palette shades 2 and 4, used for dark and light mode respectively.
fn shades(hue: Hue) -> (u32, u32) {
    match hue {
        Hue::Blue => (0x62a0ea, 0x1c71d8),
        Hue::Green => (0x57e389, 0x2ec27e),
        Hue::Yellow => (0xf8e45c, 0xe5a50a),
        Hue::Orange => (0xffa348, 0xe66100),
        Hue::Red => (0xed333b, 0xc01c28),
        Hue::Purple => (0xc061cb, 0x813d9c),
    }
}

fn rgb(hex: u32) -> RGBA {
    RGBA::new(
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
        1.0,
    )
}

fn named(hue: Hue, dark: bool) -> RGBA {
    let (d, l) = shades(hue);
    rgb(if dark { d } else { l })
}

fn themed(hue: Hue, palette: &Palette) -> RGBA {
    let Rgb(r, g, b) = match hue {
        Hue::Blue => palette.blue,
        Hue::Green => palette.green,
        Hue::Yellow => palette.yellow,
        Hue::Orange => palette.orange,
        Hue::Red => palette.red,
        Hue::Purple => palette.magenta,
    };
    RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

pub fn with_alpha(c: RGBA, alpha: f64) -> RGBA {
    let mut c = c;
    c.set_alpha(c.alpha() * alpha as f32);
    c
}

/// The colour to draw a role in: from the Omarchy palette when one is
/// loaded, otherwise from Adwaita's accent and the GNOME palette.
pub fn resolve(role: ColorRole, palette: Option<&Palette>, fg: RGBA) -> RGBA {
    let sm = adw::StyleManager::default();
    match (role, palette) {
        (ColorRole::Accent, Some(p)) => {
            let Rgb(r, g, b) = p.accent;
            RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
        }
        (ColorRole::Accent, None) => sm.accent_color_rgba(),
        (ColorRole::Named(hue), Some(p)) => themed(hue, p),
        (ColorRole::Named(hue), None) => named(hue, sm.is_dark()),
        (ColorRole::Foreground, _) => with_alpha(fg, 0.6),
    }
}
