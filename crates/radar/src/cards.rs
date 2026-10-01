use adw::prelude::*;
use radar_core::Stats;

use crate::axis::Format;
use crate::chart::{TimeSeriesChart, YRange};

pub struct Card {
    pub root: gtk::Box,
    pub header: gtk::Box,
    summary: gtk::Label,
    pub chart: TimeSeriesChart,
    format: Format,
}

impl Card {
    pub fn new(title: &str, format: Format, y_range: YRange) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.add_css_class("card");
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
        inner.set_margin_top(12);
        inner.set_margin_bottom(10);
        inner.set_margin_start(14);
        inner.set_margin_end(14);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = gtk::Label::new(Some(title));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        let summary = gtk::Label::new(None);
        summary.add_css_class("dim-label");
        summary.add_css_class("caption");
        summary.add_css_class("numeric");
        summary.set_hexpand(true);
        summary.set_xalign(1.0);
        summary.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        header.append(&title);
        header.append(&summary);

        let chart = TimeSeriesChart::new(format, y_range);
        inner.append(&header);
        inner.append(chart.widget());
        root.append(&inner);
        Card {
            root,
            header,
            summary,
            chart,
            format,
        }
    }

    pub fn set_summary(&self, points: &[(i64, Option<f64>)], stats: Option<Stats>) {
        self.summary
            .set_text(&summarize(points, stats, self.format));
    }
}

fn summarize(points: &[(i64, Option<f64>)], stats: Option<Stats>, format: Format) -> String {
    let (Some(now), Some(stats)) = (points.iter().rev().find_map(|p| p.1), stats) else {
        return String::new();
    };
    format!(
        "now {} · avg {} · max {}",
        format.format(now),
        format.format(stats.avg),
        format.format(stats.max)
    )
}
