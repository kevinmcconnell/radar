use std::cell::{Cell, RefCell};
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

pub const SUB_HOUR_PRESETS: [(&str, i64); 4] = [
    ("30m", 30 * 60),
    ("15m", 15 * 60),
    ("10m", 10 * 60),
    ("5m", 5 * 60),
];

const MINUTES_PER_DAY: i32 = 24 * 60;

pub fn preset_named(name: &str) -> Option<i64> {
    SUB_HOUR_PRESETS
        .iter()
        .chain(PRESETS.iter())
        .find(|(label, _)| *label == name)
        .map(|(_, secs)| *secs)
}

pub struct RangePicker {
    pub root: gtk::Box,
    sub_hour: gtk::ToggleButton,
    sub_hour_secs: Rc<Cell<i64>>,
    toggles: Vec<gtk::ToggleButton>,
    custom: gtk::MenuButton,
    updating: Rc<Cell<bool>>,
    current: Rc<Cell<Range>>,
    oldest: Rc<Cell<Option<i64>>>,
}

struct Endpoint {
    day: gtk::DropDown,
    labels: gtk::StringList,
    time: gtk::SpinButton,
    days: RefCell<Vec<glib::DateTime>>,
}

impl Endpoint {
    fn new() -> Self {
        let labels = gtk::StringList::new(&[]);
        let day = gtk::DropDown::builder()
            .model(&labels)
            .hexpand(true)
            .build();
        let time = gtk::SpinButton::with_range(0.0, (MINUTES_PER_DAY - 1) as f64, 5.0);
        time.set_numeric(false);
        time.set_increments(5.0, 60.0);
        time.set_width_chars(5);
        time.connect_output(|s| {
            s.set_text(&format_minutes(s.value_as_int()));
            glib::Propagation::Stop
        });
        time.connect_input(|s| Some(parse_minutes(&s.text()).map(f64::from).ok_or(())));
        Endpoint {
            day,
            labels,
            time,
            days: RefCell::new(Vec::new()),
        }
    }

    fn attach(&self, grid: &gtk::Grid, title: &str, row: i32) {
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        grid.attach(&label, 0, row, 1, 1);
        grid.attach(&self.day, 1, row, 1, 1);
        grid.attach(&self.time, 2, row, 1, 1);
    }

    fn set_days(&self, oldest: Option<i64>) {
        let Some(today) = glib::DateTime::now_local()
            .ok()
            .and_then(|now| start_of_day(&now))
        else {
            return;
        };
        let oldest = oldest.unwrap_or(i64::MAX).min(today.to_unix());
        let days: Vec<glib::DateTime> = (0..)
            .map_while(|back| today.add_days(-back).ok())
            .take_while(|day| day.add_days(1).is_ok_and(|next| next.to_unix() > oldest))
            .collect();
        let labels: Vec<String> = days
            .iter()
            .enumerate()
            .map(|(back, day)| day_label(day, back))
            .collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.labels
            .splice(0, self.labels.n_items(), labels.as_slice());
        *self.days.borrow_mut() = days;
    }

    fn set(&self, t: i64) {
        let days = self.days.borrow();
        let containing = days.iter().position(|day| day.to_unix() <= t);
        let minutes = match (containing, glib::DateTime::from_unix_local(t)) {
            (Some(_), Ok(dt)) => dt.hour() * 60 + dt.minute(),
            _ => 0,
        };
        self.day
            .set_selected(containing.unwrap_or(days.len().saturating_sub(1)) as u32);
        self.time.set_value(minutes as f64);
    }

    fn get(&self) -> Option<i64> {
        self.time.update();
        let days = self.days.borrow();
        let day = days.get(self.day.selected() as usize)?;
        let minutes = self.time.value_as_int();
        let dt = glib::DateTime::from_local(
            day.year(),
            day.month(),
            day.day_of_month(),
            minutes / 60,
            minutes % 60,
            0.0,
        )
        .ok()?;
        Some(dt.to_unix())
    }
}

fn start_of_day(dt: &glib::DateTime) -> Option<glib::DateTime> {
    glib::DateTime::from_local(dt.year(), dt.month(), dt.day_of_month(), 0, 0, 0.0).ok()
}

fn day_label(day: &glib::DateTime, days_back: usize) -> String {
    match days_back {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ => day
            .format("%a %b %-e")
            .map(|s| s.to_string())
            .unwrap_or_default(),
    }
}

fn format_minutes(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn parse_minutes(text: &str) -> Option<i32> {
    let text = text.trim();
    let (hour, minute) = text.split_once(':').unwrap_or((text, "0"));
    let (hour, minute) = (hour.parse::<i32>().ok()?, minute.parse::<i32>().ok()?);
    ((0..24).contains(&hour) && (0..60).contains(&minute)).then_some(hour * 60 + minute)
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
        let oldest = Rc::new(Cell::new(None));
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("linked");

        let (default_label, default_secs) = SUB_HOUR_PRESETS[SUB_HOUR_PRESETS.len() - 1];
        let sub_hour_secs = Rc::new(Cell::new(default_secs));
        let sub_hour = gtk::ToggleButton::with_label(default_label);
        {
            let (on_change, updating, sub_hour_secs) =
                (on_change.clone(), updating.clone(), sub_hour_secs.clone());
            sub_hour.connect_toggled(move |b| {
                if b.is_active() && !updating.get() {
                    on_change(Range::Preset(sub_hour_secs.get()));
                }
            });
        }
        root.append(&sub_hour);

        let sub_hour_menu = gtk::MenuButton::new();
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let sub_hour_popover = gtk::Popover::builder().child(&choices).build();
        for (label, secs) in SUB_HOUR_PRESETS {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            let (on_change, popover) = (on_change.clone(), sub_hour_popover.clone());
            b.connect_clicked(move |_| {
                popover.popdown();
                on_change(Range::Preset(secs));
            });
            choices.append(&b);
        }
        sub_hour_menu.set_popover(Some(&sub_hour_popover));
        root.append(&sub_hour_menu);
        {
            let sub_hour_menu = sub_hour_menu.clone();
            sub_hour.connect_toggled(move |b| mark_checked(&sub_hour_menu, b.is_active()));
        }
        {
            let (sub_hour, sub_hour_menu) = (sub_hour.clone(), sub_hour_menu.clone());
            sub_hour_popover
                .connect_closed(move |_| mark_checked(&sub_hour_menu, sub_hour.is_active()));
        }

        let mut toggles: Vec<gtk::ToggleButton> = Vec::new();
        for (label, secs) in PRESETS {
            let b = gtk::ToggleButton::with_label(label);
            b.set_group(Some(&sub_hour));
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
        let rows = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(6)
            .build();
        start.attach(&rows, "From", 0);
        end.attach(&rows, "To", 1);
        content.append(&rows);
        content.append(&error);
        apply.set_halign(gtk::Align::End);
        content.append(&apply);
        let popover = gtk::Popover::builder().child(&content).build();
        custom.set_popover(Some(&popover));
        root.append(&custom);

        let start = Rc::new(start);
        let end = Rc::new(end);
        {
            let (start, end, error, current, oldest) = (
                start.clone(),
                end.clone(),
                error.clone(),
                current.clone(),
                oldest.clone(),
            );
            popover.connect_show(move |_| {
                let now = glib::DateTime::now_local()
                    .map(|d| d.to_unix())
                    .unwrap_or(0);
                let (from, to) = current.get().bounds(now);
                start.set_days(oldest.get());
                end.set_days(oldest.get());
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
            sub_hour,
            sub_hour_secs,
            toggles,
            custom,
            updating,
            current,
            oldest,
        }
    }

    pub fn set_oldest(&self, oldest: Option<i64>) {
        self.oldest.set(oldest);
    }

    pub fn show(&self, range: Range) {
        self.current.set(range);
        self.updating.set(true);
        let sub_hour = match range {
            Range::Preset(secs) => SUB_HOUR_PRESETS.iter().find(|(_, s)| *s == secs),
            Range::Custom { .. } => None,
        };
        if let Some((label, secs)) = sub_hour {
            self.sub_hour.set_label(label);
            self.sub_hour_secs.set(*secs);
        }
        self.sub_hour.set_active(sub_hour.is_some());
        match range {
            Range::Preset(secs) => {
                for (b, (_, s)) in self.toggles.iter().zip(PRESETS) {
                    b.set_active(s == secs);
                }
                self.custom.set_label("Custom…");
                highlight(&self.custom, false);
            }
            Range::Custom { from, to } => {
                for b in &self.toggles {
                    b.set_active(false);
                }
                self.custom.set_label(&describe(from, to));
                highlight(&self.custom, true);
            }
        }
        self.updating.set(false);
    }
}

fn mark_checked(menu: &gtk::MenuButton, checked: bool) {
    let Some(inner_button) = menu.first_child() else {
        return;
    };
    if checked {
        inner_button.set_state_flags(gtk::StateFlags::CHECKED, false);
    } else {
        inner_button.unset_state_flags(gtk::StateFlags::CHECKED);
    }
}

fn highlight(menu: &gtk::MenuButton, highlighted: bool) {
    let Some(inner_button) = menu.first_child() else {
        return;
    };
    if highlighted {
        inner_button.add_css_class("suggested-action");
    } else {
        inner_button.remove_css_class("suggested-action");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_found_by_name() {
        assert_eq!(preset_named("5m"), Some(300));
        assert_eq!(preset_named("30m"), Some(1800));
        assert_eq!(preset_named("1h"), Some(3600));
        assert_eq!(preset_named("5d"), Some(5 * 86400));
        assert_eq!(preset_named("2h"), None);
    }

    #[test]
    fn minutes_format_as_clock_time() {
        assert_eq!(format_minutes(0), "00:00");
        assert_eq!(format_minutes(9 * 60 + 5), "09:05");
        assert_eq!(format_minutes(MINUTES_PER_DAY - 1), "23:59");
    }

    #[test]
    fn clock_time_parses_to_minutes() {
        assert_eq!(parse_minutes("09:05"), Some(545));
        assert_eq!(parse_minutes(" 9:05 "), Some(545));
        assert_eq!(parse_minutes("14"), Some(840));
        assert_eq!(parse_minutes("23:59"), Some(1439));
    }

    #[test]
    fn invalid_clock_time_does_not_parse() {
        assert_eq!(parse_minutes("24:00"), None);
        assert_eq!(parse_minutes("12:60"), None);
        assert_eq!(parse_minutes("-1:00"), None);
        assert_eq!(parse_minutes("9:"), None);
        assert_eq!(parse_minutes("noon"), None);
    }
}
