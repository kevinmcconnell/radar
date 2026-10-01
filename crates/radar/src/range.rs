use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Preset(i64),
    Custom { from: i64, to: i64 },
}

impl Range {
    pub fn bounds(self, now: i64) -> (i64, i64) {
        match self {
            Range::Preset(secs) => (now - secs, now),
            Range::Custom { from, to } => (from, to),
        }
    }
}

pub const PRESETS: [(&str, i64); 5] = [
    ("1h", 3600),
    ("6h", 6 * 3600),
    ("24h", 86400),
    ("3d", 3 * 86400),
    ("5d", 5 * 86400),
];

pub struct RangePicker {
    pub root: gtk::Box,
    toggles: Vec<gtk::ToggleButton>,
    custom: gtk::MenuButton,
    updating: Rc<Cell<bool>>,
    current: Rc<Cell<Range>>,
}

struct Endpoint {
    calendar: gtk::Calendar,
    hour: gtk::SpinButton,
    minute: gtk::SpinButton,
}

impl Endpoint {
    fn new() -> Self {
        let spin = |max: f64| {
            let s = gtk::SpinButton::with_range(0.0, max, 1.0);
            s.set_orientation(gtk::Orientation::Vertical);
            s.set_numeric(true);
            s.connect_output(|s| {
                s.set_text(&format!("{:02}", s.value() as i64));
                glib::Propagation::Stop
            });
            s
        };
        Endpoint {
            calendar: gtk::Calendar::new(),
            hour: spin(23.0),
            minute: spin(59.0),
        }
    }

    fn widget(&self, title: &str) -> gtk::Box {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let label = gtk::Label::new(Some(title));
        label.add_css_class("heading");
        label.set_xalign(0.0);
        col.append(&label);
        col.append(&self.calendar);
        let time = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        time.set_halign(gtk::Align::Center);
        time.append(&self.hour);
        time.append(&gtk::Label::new(Some(":")));
        time.append(&self.minute);
        col.append(&time);
        col
    }

    fn set(&self, t: i64) {
        if let Ok(dt) = glib::DateTime::from_unix_local(t) {
            self.calendar.select_day(&dt);
            self.hour.set_value(dt.hour() as f64);
            self.minute.set_value(dt.minute() as f64);
        }
    }

    fn get(&self) -> Option<i64> {
        let d = self.calendar.date();
        let dt = glib::DateTime::from_local(
            d.year(),
            d.month(),
            d.day_of_month(),
            self.hour.value() as i32,
            self.minute.value() as i32,
            0.0,
        )
        .ok()?;
        Some(dt.to_unix())
    }
}

fn describe(from: i64, to: i64) -> String {
    let fmt = |t: i64, f: &str| {
        glib::DateTime::from_unix_local(t)
            .ok()
            .and_then(|d| d.format(f).ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let same_day = fmt(from, "%F") == fmt(to, "%F");
    if same_day {
        format!(
            "{} {}–{}",
            fmt(from, "%b %-e"),
            fmt(from, "%H:%M"),
            fmt(to, "%H:%M")
        )
    } else {
        format!(
            "{} – {}",
            fmt(from, "%b %-e %H:%M"),
            fmt(to, "%b %-e %H:%M")
        )
    }
}

impl RangePicker {
    pub fn new(on_change: impl Fn(Range) + 'static) -> Self {
        let on_change = Rc::new(on_change);
        let updating = Rc::new(Cell::new(false));
        let current = Rc::new(Cell::new(Range::Preset(PRESETS[0].1)));
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("linked");

        let mut toggles: Vec<gtk::ToggleButton> = Vec::new();
        for (label, secs) in PRESETS {
            let b = gtk::ToggleButton::with_label(label);
            if let Some(first) = toggles.first() {
                b.set_group(Some(first));
            }
            let (on_change, updating) = (on_change.clone(), updating.clone());
            b.connect_toggled(move |b| {
                if b.is_active() && !updating.get() {
                    on_change(Range::Preset(secs));
                }
            });
            root.append(&b);
            toggles.push(b);
        }

        let custom = gtk::MenuButton::builder()
            .label("Custom…")
            .always_show_arrow(false)
            .build();
        let start = Endpoint::new();
        let end = Endpoint::new();
        let apply = gtk::Button::with_label("Show Range");
        apply.add_css_class("suggested-action");
        let error = gtk::Label::new(Some("The end must be after the start"));
        error.add_css_class("error");
        error.set_visible(false);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let cols = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        cols.append(&start.widget("From"));
        cols.append(&end.widget("To"));
        content.append(&cols);
        content.append(&error);
        apply.set_halign(gtk::Align::End);
        content.append(&apply);
        let popover = gtk::Popover::builder().child(&content).build();
        custom.set_popover(Some(&popover));
        root.append(&custom);

        let start = Rc::new(start);
        let end = Rc::new(end);
        {
            let (start, end, error, current) =
                (start.clone(), end.clone(), error.clone(), current.clone());
            popover.connect_show(move |_| {
                let now = glib::DateTime::now_local()
                    .map(|d| d.to_unix())
                    .unwrap_or(0);
                let (from, to) = current.get().bounds(now);
                start.set(from);
                end.set(to);
                error.set_visible(false);
            });
        }
        {
            let popover = popover.clone();
            apply.connect_clicked(move |_| match (start.get(), end.get()) {
                (Some(from), Some(to)) if to > from => {
                    error.set_visible(false);
                    popover.popdown();
                    on_change(Range::Custom { from, to });
                }
                _ => error.set_visible(true),
            });
        }

        RangePicker {
            root,
            toggles,
            custom,
            updating,
            current,
        }
    }

    pub fn show(&self, range: Range) {
        self.current.set(range);
        self.updating.set(true);
        match range {
            Range::Preset(secs) => {
                for (b, (_, s)) in self.toggles.iter().zip(PRESETS) {
                    b.set_active(s == secs);
                }
                self.custom.set_label("Custom…");
                self.custom.remove_css_class("suggested-action");
            }
            Range::Custom { from, to } => {
                for b in &self.toggles {
                    b.set_active(false);
                }
                self.custom.set_label(&describe(from, to));
                self.custom.add_css_class("suggested-action");
            }
        }
        self.updating.set(false);
    }
}
