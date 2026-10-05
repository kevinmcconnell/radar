//! Follow the Omarchy theme. Omarchy 4 keeps the active theme in
//! `~/.local/state/omarchy/current/theme/`, a directory it swaps atomically
//! on every theme change, and describes the palette in `colors.toml` there.
//! When that file exists, its colours are mapped onto the CSS variables that
//! libadwaita styles every widget from, and the light/dark scheme is forced
//! to match. Without it, the app stays on stock Adwaita and follows the
//! desktop's dark/light preference.
//!
//! Apps that draw their own colours (charts, canvases) can read the loaded
//! [`Palette`] from the [`Theme`] handle and subscribe to changes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

/// Directory whose `theme` child Omarchy replaces on a theme change.
fn omarchy_current_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/state/omarchy/current"))
}

/// Whether this machine runs Omarchy, whose window manager closes windows
/// itself, so apps can leave out their own close button.
pub fn is_installed() -> bool {
    omarchy_current_dir().is_some_and(|p| p.is_dir())
}

type Handler = Rc<dyn Fn(Option<&Palette>)>;

/// A handle on the followed theme. Keep it alive for as long as the app
/// should track theme changes; dropping it stops the directory watch.
#[derive(Clone)]
pub struct Theme {
    inner: Rc<Inner>,
}

struct Inner {
    palette: RefCell<Option<Palette>>,
    handlers: RefCell<Vec<Handler>>,
    provider: gtk::CssProvider,
    theme_dir: PathBuf,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

impl Theme {
    /// The palette currently applied, or `None` when the app is on stock
    /// Adwaita because this machine has no Omarchy theme.
    pub fn palette(&self) -> Option<Palette> {
        self.inner.palette.borrow().clone()
    }

    /// Call `f` after every theme change with the new palette, or `None`
    /// when the theme went away and the app is back on stock Adwaita.
    pub fn connect_changed(&self, f: impl Fn(Option<&Palette>) + 'static) {
        self.inner.handlers.borrow_mut().push(Rc::new(f));
    }
}

/// Start following the Omarchy theme. Must be called once GTK has a default
/// display, typically from the application's `startup` or `activate` signal.
/// On a machine without Omarchy the handle simply reports no palette.
pub fn follow() -> Theme {
    let current = omarchy_current_dir().filter(|p| p.is_dir());
    let provider = gtk::CssProvider::new();
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let theme_dir = current
        .as_ref()
        .map(|c| c.join("theme"))
        .unwrap_or_default();
    let inner = Rc::new(Inner {
        palette: RefCell::new(None),
        handlers: RefCell::new(Vec::new()),
        provider,
        theme_dir,
        monitor: RefCell::new(None),
    });
    let Some(current) = current else {
        return Theme { inner };
    };
    inner.apply();
    if let Some(monitor) = watch(&current, Rc::downgrade(&inner)) {
        *inner.monitor.borrow_mut() = Some(monitor);
    }
    Theme { inner }
}

fn watch(current: &Path, inner: Weak<Inner>) -> Option<gio::FileMonitor> {
    let monitor = gio::File::for_path(current)
        .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        .ok()?;
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    monitor.connect_changed(move |_, file, _, _| {
        let is_theme = file
            .basename()
            .is_some_and(|n| n == Path::new("theme") || n == Path::new("theme.name"));
        if !is_theme {
            return;
        }
        // A theme switch produces a burst of events; apply once it settles.
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let inner = inner.clone();
        let slot = pending.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(200), move || {
            slot.borrow_mut().take();
            if let Some(inner) = inner.upgrade() {
                inner.apply();
            }
        });
        *pending.borrow_mut() = Some(id);
    });
    Some(monitor)
}

impl Inner {
    fn apply(&self) {
        let style = adw::StyleManager::default();
        let palette = Palette::load(&self.theme_dir);
        match &palette {
            Some(palette) => {
                style.set_color_scheme(match palette.mode {
                    Mode::Dark => adw::ColorScheme::ForceDark,
                    Mode::Light => adw::ColorScheme::ForceLight,
                });
                self.provider.load_from_string(&palette.css());
            }
            None => {
                style.set_color_scheme(adw::ColorScheme::Default);
                self.provider.load_from_string("");
            }
        }
        *self.palette.borrow_mut() = palette.clone();
        let handlers = self.handlers.borrow().clone();
        for handler in &handlers {
            handler(palette.as_ref());
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_remove_provider_for_display(&display, &self.provider);
        }
        if self.palette.borrow().is_some() {
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Accepts `#rrggbb` only, the form Omarchy's own colour maths requires.
    fn parse(s: &str) -> Option<Rgb> {
        let hex = s.strip_prefix('#')?.as_bytes();
        if hex.len() != 6 || !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let byte =
            |i: usize| u8::from_str_radix(std::str::from_utf8(&hex[i..i + 2]).ok()?, 16).ok();
        Some(Rgb(byte(0)?, byte(2)?, byte(4)?))
    }

    fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// Blend towards `other` by `amount` in 0..=1, like Omarchy's mix_color.
    fn mix(self, other: Rgb, amount: f32) -> Rgb {
        let ch = |a: u8, b: u8| (a as f32 * (1.0 - amount) + b as f32 * amount + 0.5) as u8;
        Rgb(
            ch(self.0, other.0),
            ch(self.1, other.1),
            ch(self.2, other.2),
        )
    }

    /// Perceived brightness in 0..=1 (sRGB relative luminance).
    fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    /// WCAG contrast ratio against `other`, from 1 (equal) to 21.
    fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance() + 0.05, other.luminance() + 0.05);
        a.max(b) / a.min(b)
    }

    /// Text colour that reads best on top of this colour, from the two
    /// Adwaita uses on filled buttons: white, or near-black at 80%.
    fn contrasting_fg(self) -> &'static str {
        let near_black = self.mix(Rgb(0, 0, 6), 0.8);
        if self.contrast(near_black) > self.contrast(WHITE) {
            "rgb(0 0 6 / 80%)"
        } else {
            "#ffffff"
        }
    }
}

const BLACK: Rgb = Rgb(0, 0, 0);
const WHITE: Rgb = Rgb(255, 255, 255);

/// The colours the client needs, resolved from an Omarchy `colors.toml` with
/// the same fallbacks Omarchy's own `omarchy-theme-color` applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub mode: Mode,
    pub background: Rgb,
    pub foreground: Rgb,
    pub accent: Rgb,
    pub dark_background: Rgb,
    pub lighter_background: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub orange: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
}

impl Palette {
    /// Read `colors.toml` from an Omarchy theme directory. `None` when there
    /// is no readable palette or it lacks a usable background.
    pub fn load(theme_dir: &Path) -> Option<Palette> {
        let text = fs::read_to_string(theme_dir.join("colors.toml")).ok()?;
        let legacy_light = theme_dir.join("light.mode").exists();
        Palette::from_colors_toml(&text, legacy_light)
    }

    pub fn from_colors_toml(text: &str, legacy_light_marker: bool) -> Option<Palette> {
        let raw = parse_colors(text);
        let color = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| raw.get(*k).and_then(|v| Rgb::parse(v)))
        };

        let background = color(&["background", "bg", "color0"])?;
        let foreground = color(&["foreground", "fg", "color7"]).unwrap_or(
            if background.contrast(BLACK) > background.contrast(WHITE) {
                BLACK
            } else {
                WHITE
            },
        );
        let mode = match raw
            .get("mode")
            .or_else(|| raw.get("theme_type"))
            .map(String::as_str)
        {
            Some("light") => Mode::Light,
            Some("dark") => Mode::Dark,
            _ if legacy_light_marker => Mode::Light,
            _ => {
                let sum = background.0 as u32 + background.1 as u32 + background.2 as u32;
                if sum > 382 { Mode::Light } else { Mode::Dark }
            }
        };
        let blue = color(&["blue", "color4"]).unwrap_or(Rgb(0x35, 0x84, 0xe4));
        let accent = color(&["accent"]).unwrap_or(blue);
        let yellow = color(&["yellow", "color3"]).unwrap_or(Rgb(0xe5, 0xa5, 0x0a));
        let dark_background =
            color(&["dark_background", "dark_bg"]).unwrap_or_else(|| background.mix(BLACK, 0.25));
        let lighter_background = color(&["lighter_background", "lighter_bg"]).unwrap_or(background);
        Some(Palette {
            mode,
            background,
            foreground,
            accent,
            dark_background,
            lighter_background,
            red: color(&["red", "color1"]).unwrap_or(Rgb(0xe0, 0x1b, 0x24)),
            green: color(&["green", "color2"]).unwrap_or(Rgb(0x2e, 0xc2, 0x7e)),
            yellow,
            orange: color(&["orange"]).unwrap_or(yellow),
            blue,
            magenta: color(&["magenta", "purple", "color5"]).unwrap_or(Rgb(0x91, 0x41, 0xac)),
            cyan: color(&["cyan", "color6"]).unwrap_or(Rgb(0x33, 0xc7, 0xde)),
        })
    }

    /// CSS that overrides the libadwaita palette variables. The standalone
    /// colours (`--accent-color` etc.) are left alone: Adwaita derives them
    /// from these background colours.
    pub fn css(&self) -> String {
        let (window, view, headerbar, raised) = match self.mode {
            Mode::Dark => (
                self.background,
                self.dark_background,
                self.lighter_background,
                self.lighter_background,
            ),
            Mode::Light => (
                self.background,
                self.background.mix(WHITE, 0.5),
                self.background,
                self.lighter_background,
            ),
        };
        let fg = self.foreground.css();
        let mut css = String::from(":root {\n");
        let mut var = |name: &str, value: &str| {
            css.push_str(&format!("  --{name}: {value};\n"));
        };
        var("accent-bg-color", &self.accent.css());
        var("accent-fg-color", self.accent.contrasting_fg());
        var("destructive-bg-color", &self.red.css());
        var("destructive-fg-color", self.red.contrasting_fg());
        var("error-bg-color", &self.red.css());
        var("error-fg-color", self.red.contrasting_fg());
        var("success-bg-color", &self.green.css());
        var("success-fg-color", self.green.contrasting_fg());
        var("warning-bg-color", &self.yellow.css());
        var("warning-fg-color", self.yellow.contrasting_fg());
        var("window-bg-color", &window.css());
        var("window-fg-color", &fg);
        var("view-bg-color", &view.css());
        var("view-fg-color", &fg);
        var("headerbar-bg-color", &headerbar.css());
        var("headerbar-fg-color", &fg);
        var("headerbar-border-color", &fg);
        var("headerbar-backdrop-color", &window.css());
        var("sidebar-bg-color", &headerbar.css());
        var("sidebar-fg-color", &fg);
        var("sidebar-backdrop-color", &window.css());
        var("secondary-sidebar-bg-color", &window.css());
        var("secondary-sidebar-fg-color", &fg);
        var("secondary-sidebar-backdrop-color", &window.css());
        var("card-bg-color", &raised.css());
        var("card-fg-color", &fg);
        var("dialog-bg-color", &raised.css());
        var("dialog-fg-color", &fg);
        var("popover-bg-color", &raised.css());
        var("popover-fg-color", &fg);
        var("thumbnail-bg-color", &raised.css());
        var("thumbnail-fg-color", &fg);
        css.push_str("}\n");
        css
    }
}

/// Parse Omarchy's `colors.toml` the way `omarchy-theme-color` does: one
/// `key = value` per line, quotes stripped, anything after the closing quote
/// (an inline comment) dropped. Keys and values that Omarchy would reject are
/// skipped too.
fn parse_colors(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key: String = key
            .chars()
            .filter(|c| !matches!(c, '"' | '\'' | ' '))
            .collect();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let value = match value.find(['"', '\'']) {
            Some(open) => {
                let rest = &value[open + 1..];
                rest.find(['"', '\'']).map_or(rest, |close| &rest[..close])
            }
            None => value.trim(),
        };
        out.insert(key, value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATPPUCCIN: &str = r##"
mode = "dark"

accent = "#89b4fa"
background = "#1e1e2e"
dark_background = "#161622"
lighter_background = "#313244"
foreground = "#cdd6f4"
red = "#f38ba8"
yellow = "#f9e2af"
green = "#a6e3a1"
blue = "#89b4fa"
"##;

    #[test]
    fn parses_quoted_values_and_inline_comments() {
        let raw = parse_colors(
            "accent = \"#644AC9\" # Purple\ncolor0 = '#FFFBEB'\n# comment = \"x\"\nbare = 12deg\n",
        );
        assert_eq!(raw["accent"], "#644AC9");
        assert_eq!(raw["color0"], "#FFFBEB");
        assert_eq!(raw["bare"], "12deg");
        assert!(!raw.contains_key("#comment"));
        assert!(!raw.contains_key("comment"));
    }

    #[test]
    fn resolves_a_semantic_palette() {
        let p = Palette::from_colors_toml(CATPPUCCIN, false).unwrap();
        assert_eq!(p.mode, Mode::Dark);
        assert_eq!(p.accent, Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(p.dark_background, Rgb(0x16, 0x16, 0x22));
        assert_eq!(p.lighter_background, Rgb(0x31, 0x32, 0x44));
        assert_eq!(p.blue, Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(p.orange, p.yellow);
        assert_eq!(p.magenta, Rgb(0x91, 0x41, 0xac));
    }

    #[test]
    fn falls_back_to_ansi_names_and_derived_shades() {
        let toml = "color0 = \"#202020\"\ncolor7 = \"#e0e0e0\"\ncolor4 = \"#5080ff\"\n";
        let p = Palette::from_colors_toml(toml, false).unwrap();
        assert_eq!(p.background, Rgb(0x20, 0x20, 0x20));
        assert_eq!(p.foreground, Rgb(0xe0, 0xe0, 0xe0));
        assert_eq!(p.accent, Rgb(0x50, 0x80, 0xff));
        assert_eq!(p.dark_background, Rgb(0x20, 0x20, 0x20).mix(BLACK, 0.25));
        assert_eq!(p.mode, Mode::Dark);
    }

    #[test]
    fn mode_comes_from_key_then_marker_then_luminance() {
        let light = "background = \"#FFFBEB\"\nmode = \"light\"\n";
        assert_eq!(
            Palette::from_colors_toml(light, false).unwrap().mode,
            Mode::Light
        );
        let dark_keyed = "background = \"#FFFBEB\"\nmode = \"dark\"\n";
        assert_eq!(
            Palette::from_colors_toml(dark_keyed, true).unwrap().mode,
            Mode::Dark
        );
        let marker = "background = \"#101010\"\n";
        assert_eq!(
            Palette::from_colors_toml(marker, true).unwrap().mode,
            Mode::Light
        );
        let bright = "background = \"#FFFBEB\"\n";
        assert_eq!(
            Palette::from_colors_toml(bright, false).unwrap().mode,
            Mode::Light
        );
        let dim = "background = \"#1e1e2e\"\n";
        assert_eq!(
            Palette::from_colors_toml(dim, false).unwrap().mode,
            Mode::Dark
        );
    }

    #[test]
    fn missing_foreground_picks_the_higher_contrast() {
        let fg = |bg: &str| {
            Palette::from_colors_toml(&format!("background = \"{bg}\"\n"), false)
                .unwrap()
                .foreground
        };
        assert_eq!(fg("#bbbbbb"), BLACK);
        assert_eq!(fg("#1e1e2e"), WHITE);
        assert_eq!(fg("#fffbeb"), BLACK);
    }

    #[test]
    fn missing_background_means_no_palette() {
        assert!(Palette::from_colors_toml("accent = \"#ff0000\"\n", false).is_none());
        assert!(Palette::from_colors_toml("background = \"blue\"\n", false).is_none());
    }

    #[test]
    fn css_maps_palette_onto_adwaita_variables() {
        let css = Palette::from_colors_toml(CATPPUCCIN, false).unwrap().css();
        assert!(css.contains("--accent-bg-color: #89b4fa;"));
        assert!(css.contains("--accent-fg-color: rgb(0 0 6 / 80%);"));
        assert!(css.contains("--window-bg-color: #1e1e2e;"));
        assert!(css.contains("--view-bg-color: #161622;"));
        assert!(css.contains("--headerbar-bg-color: #313244;"));
        assert!(css.contains("--window-fg-color: #cdd6f4;"));
        assert!(css.contains("--destructive-bg-color: #f38ba8;"));
        assert!(!css.contains("--accent-color:"));
    }

    #[test]
    fn text_colour_maximises_contrast() {
        assert_eq!(Rgb(0x64, 0x4a, 0xc9).contrasting_fg(), "#ffffff");
        assert_eq!(Rgb(0xf9, 0xe2, 0xaf).contrasting_fg(), "rgb(0 0 6 / 80%)");
        assert_eq!(Rgb(0x7a, 0xa2, 0xf7).contrasting_fg(), "rgb(0 0 6 / 80%)");
        assert_eq!(Rgb(0x99, 0x99, 0x99).contrasting_fg(), "rgb(0 0 6 / 80%)");
    }

    #[test]
    fn rejects_malformed_colours_without_panicking() {
        assert_eq!(Rgb::parse("#a\u{e9}xxx"), None);
        assert_eq!(Rgb::parse("#1e1e2e80"), None);
        assert_eq!(Rgb::parse("#fff"), None);
        assert_eq!(Rgb::parse("1e1e2e"), None);
        assert_eq!(Rgb::parse("#1E1e2E"), Some(Rgb(0x1e, 0x1e, 0x2e)));
        assert!(Palette::from_colors_toml("background = \"#a\u{e9}xxx\"\n", false).is_none());
    }
}
