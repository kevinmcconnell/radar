use adw::prelude::*;
use radar_core::{Meta, ProcUsage};

use crate::axis;

pub struct ProcPanel {
    pub root: gtk::Box,
    list: gtk::ListBox,
}

impl ProcPanel {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let title = gtk::Label::new(Some("Top Processes"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let placeholder = gtk::Label::new(Some("No process activity in this range"));
        placeholder.add_css_class("dim-label");
        placeholder.set_margin_top(18);
        placeholder.set_margin_bottom(18);
        list.set_placeholder(Some(&placeholder));
        root.append(&title);
        root.append(&list);
        ProcPanel { root, list }
    }

    pub fn set(&self, procs: &[ProcUsage], meta: &Meta, range_secs: i64) {
        self.list.remove_all();
        let top = procs.first().map_or(1, |p| p.ticks.max(1)) as f64;
        let capacity = (meta.ncpus.max(1) * range_secs.max(1)) as f64;
        for p in procs {
            let cpu_secs = p.ticks as f64 / meta.clk_tck.max(1) as f64;
            self.list.append(&row(
                &p.name,
                cpu_secs,
                cpu_secs / capacity * 100.0,
                p.ticks as f64 / top,
            ));
        }
    }
}

fn row(name: &str, cpu_secs: f64, percent: f64, fraction: f64) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    row.set_margin_top(8);
    row.set_margin_bottom(10);
    row.set_margin_start(12);
    row.set_margin_end(12);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name_label = gtk::Label::new(Some(name));
    name_label.set_xalign(0.0);
    name_label.set_hexpand(true);
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name_label.set_tooltip_text(Some(name));
    let time = gtk::Label::new(Some(&axis::duration(cpu_secs)));
    time.add_css_class("numeric");
    let pct = gtk::Label::new(Some(&format_percent(percent)));
    pct.add_css_class("numeric");
    pct.add_css_class("dim-label");
    pct.set_width_chars(6);
    pct.set_xalign(1.0);
    pct.set_tooltip_text(Some("Share of total CPU capacity over the range"));
    line.append(&name_label);
    line.append(&time);
    line.append(&pct);

    let bar = gtk::ProgressBar::new();
    bar.set_fraction(fraction.clamp(0.0, 1.0));
    row.append(&line);
    row.append(&bar);
    row
}

fn format_percent(p: f64) -> String {
    if p >= 10.0 {
        format!("{p:.0}%")
    } else if p >= 0.1 {
        format!("{p:.1}%")
    } else {
        format!("{p:.2}%")
    }
}
