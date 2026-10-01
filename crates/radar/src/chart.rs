use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, gdk, glib, pango};

use crate::axis::{self, Format};
use crate::palette::{self, ColorRole, with_alpha};
use omarchy_theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Fill,
    Line,
    Dashed,
}

#[derive(Debug, Clone)]
pub struct Series {
    pub label: String,
    pub points: Vec<(i64, Option<f64>)>,
    pub style: Style,
    pub color: ColorRole,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum YRange {
    Fixed(f64),
    Auto,
}

#[derive(Debug, Clone, Copy, Default)]
struct Plot {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

struct Data {
    from: i64,
    to: i64,
    bucket: i64,
    series: Vec<Series>,
    breaks: Rc<[i64]>,
    y_range: YRange,
    format: Format,
    plot: Plot,
    hover_x: Option<f64>,
    drag: Option<(f64, f64)>,
    on_zoom: Option<Rc<dyn Fn(i64, i64)>>,
    palette: Option<Palette>,
}

impl Data {
    fn x_of(&self, t: f64) -> f64 {
        let span = (self.to - self.from).max(1) as f64;
        self.plot.left + (t - self.from as f64) / span * self.plot.width
    }

    fn t_of(&self, x: f64) -> i64 {
        let span = (self.to - self.from).max(1) as f64;
        let frac = ((x - self.plot.left) / self.plot.width.max(1.0)).clamp(0.0, 1.0);
        self.from + (frac * span) as i64
    }

    fn in_plot(&self, x: f64) -> bool {
        x >= self.plot.left && x <= self.plot.left + self.plot.width
    }
}

#[derive(Clone)]
pub struct TimeSeriesChart {
    area: gtk::DrawingArea,
    data: Rc<RefCell<Data>>,
}

const FILL_TOP_ALPHA: f64 = 0.35;
const FILL_BOTTOM_ALPHA: f64 = 0.05;
const LABEL_ALPHA: f64 = 0.55;
const GRID_ALPHA: f64 = 0.12;

impl TimeSeriesChart {
    pub fn new(format: Format, y_range: YRange) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(150)
            .hexpand(true)
            .has_tooltip(true)
            .build();
        let data = Rc::new(RefCell::new(Data {
            from: 0,
            to: 1,
            bucket: 5,
            series: Vec::new(),
            breaks: Rc::new([]),
            y_range,
            format,
            plot: Plot::default(),
            hover_x: None,
            drag: None,
            on_zoom: None,
            palette: None,
        }));
        let chart = TimeSeriesChart { area, data };
        chart.setup();
        chart
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// `breaks` holds bucket times that start after a collection gap; lines never join across them.
    pub fn set_data(
        &self,
        from: i64,
        to: i64,
        bucket: i64,
        series: Vec<Series>,
        breaks: Rc<[i64]>,
    ) {
        {
            let mut d = self.data.borrow_mut();
            d.breaks = breaks;
            d.from = from;
            d.to = to.max(from + 1);
            d.bucket = bucket.max(1);
            d.series = series;
        }
        self.area.queue_draw();
    }

    pub fn connect_zoom(&self, f: impl Fn(i64, i64) + 'static) {
        self.data.borrow_mut().on_zoom = Some(Rc::new(f));
    }

    pub fn queue_draw(&self) {
        self.area.queue_draw();
    }

    pub fn set_palette(&self, palette: Option<Palette>) {
        self.data.borrow_mut().palette = palette;
        self.area.queue_draw();
    }

    fn setup(&self) {
        let data = self.data.clone();
        self.area.set_draw_func(move |area, cr, w, h| {
            draw(area, cr, w as f64, h as f64, &mut data.borrow_mut());
        });

        let motion = gtk::EventControllerMotion::new();
        let (data, area) = (self.data.clone(), self.area.clone());
        motion.connect_motion(move |_, x, _| {
            data.borrow_mut().hover_x = Some(x);
            area.queue_draw();
        });
        let (data, area) = (self.data.clone(), self.area.clone());
        motion.connect_leave(move |_| {
            data.borrow_mut().hover_x = None;
            area.queue_draw();
        });
        self.area.add_controller(motion);

        let data = self.data.clone();
        self.area
            .connect_query_tooltip(move |_, x, _, keyboard, tooltip| {
                if keyboard {
                    return false;
                }
                let d = data.borrow();
                if d.series.is_empty() || !d.in_plot(x as f64) {
                    return false;
                }
                tooltip.set_markup(Some(&tooltip_markup(&d, d.t_of(x as f64))));
                true
            });

        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let (data, area) = (self.data.clone(), self.area.clone());
        drag.connect_drag_begin(move |_, x, _| {
            let mut d = data.borrow_mut();
            if d.in_plot(x) {
                d.drag = Some((x, x));
                area.queue_draw();
            }
        });
        let (data, area) = (self.data.clone(), self.area.clone());
        drag.connect_drag_update(move |_, dx, _| {
            let mut d = data.borrow_mut();
            if let Some((x0, _)) = d.drag {
                let max = d.plot.left + d.plot.width;
                d.drag = Some((x0, (x0 + dx).clamp(d.plot.left, max)));
                area.queue_draw();
            }
        });
        let (data, area) = (self.data.clone(), self.area.clone());
        drag.connect_drag_end(move |_, _, _| {
            let zoom = {
                let mut d = data.borrow_mut();
                let Some((x0, x1)) = d.drag.take() else {
                    return;
                };
                area.queue_draw();
                let (a, b) = (x0.min(x1), x0.max(x1));
                if b - a < 8.0 {
                    return;
                }
                let (from, to) = (d.t_of(a), d.t_of(b));
                d.on_zoom.clone().map(|f| (f, from, to))
            };
            if let Some((f, from, to)) = zoom {
                f(from, to);
            }
        });
        self.area.add_controller(drag);
    }
}

fn set_color(cr: &cairo::Context, c: &gdk::RGBA) {
    cr.set_source_rgba(
        c.red() as f64,
        c.green() as f64,
        c.blue() as f64,
        c.alpha() as f64,
    );
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, width: f64, height: f64, d: &mut Data) {
    let fg = area.color();
    let hc = adw::StyleManager::default().is_high_contrast();
    let line_width = if hc { 2.5 } else { 1.75 };

    let layout = area.create_pango_layout(None);
    let mut font = area.pango_context().font_description().unwrap_or_default();
    let small = (font.size() as f64 * 0.85) as i32;
    if font.is_size_absolute() {
        font.set_absolute_size(small as f64);
    } else {
        font.set_size(small);
    }
    layout.set_font_description(Some(&font));
    layout.set_text("0");
    let text_h = layout.pixel_size().1 as f64;

    let legend_h = if d.series.len() > 1 {
        draw_legend(cr, &layout, d, width, fg, text_h)
    } else {
        0.0
    };

    let data_max = d
        .series
        .iter()
        .flat_map(|s| s.points.iter().filter_map(|p| p.1))
        .fold(0.0f64, f64::max);
    let (top_value, ticks) = match d.y_range {
        YRange::Fixed(max) => (max, axis::fixed_ticks(max)),
        YRange::Auto => axis::y_ticks(data_max, d.format),
    };

    let labels: Vec<String> = ticks.iter().map(|&v| d.format.format(v)).collect();
    let label_w = labels
        .iter()
        .map(|l| {
            layout.set_text(l);
            layout.pixel_size().0 as f64
        })
        .fold(0.0, f64::max);

    d.plot = Plot {
        left: (label_w + 10.0).round(),
        top: legend_h + (text_h / 2.0).ceil(),
        width: (width - label_w - 10.0 - 8.0).max(1.0),
        height: (height - legend_h - text_h * 1.5 - 8.0).max(1.0),
    };
    let p = d.plot;
    let bottom = p.top + p.height;
    let y_of = |v: f64| bottom - (v / top_value).clamp(0.0, 1.05) * p.height;

    cr.set_line_width(1.0);
    for (v, label) in ticks.iter().zip(&labels) {
        let y = y_of(*v).round() + 0.5;
        set_color(cr, &with_alpha(fg, GRID_ALPHA));
        cr.move_to(p.left, y);
        cr.line_to(p.left + p.width, y);
        let _ = cr.stroke();

        layout.set_text(label);
        let (lw, lh) = layout.pixel_size();
        set_color(cr, &with_alpha(fg, LABEL_ALPHA));
        cr.move_to(p.left - 6.0 - lw as f64, y - lh as f64 / 2.0);
        pangocairo::functions::show_layout(cr, &layout);
    }

    draw_time_axis(cr, &layout, d, fg, bottom);

    cr.save().ok();
    cr.rectangle(p.left, p.top - 2.0, p.width, p.height + 2.0);
    cr.clip();
    for s in &d.series {
        draw_series(
            cr,
            d,
            s,
            palette::resolve(s.color, d.palette.as_ref(), fg),
            line_width,
            &y_of,
            bottom,
        );
    }
    cr.restore().ok();

    if let Some(x) = d.hover_x.filter(|&x| d.in_plot(x)) {
        let x = x.round() + 0.5;
        set_color(cr, &with_alpha(fg, 0.35));
        cr.set_line_width(1.0);
        cr.move_to(x, p.top);
        cr.line_to(x, bottom);
        let _ = cr.stroke();
    }

    if let Some((x0, x1)) = d.drag {
        let accent = palette::resolve(ColorRole::Accent, d.palette.as_ref(), fg);
        set_color(cr, &with_alpha(accent, 0.2));
        cr.rectangle(x0.min(x1), p.top, (x1 - x0).abs(), p.height);
        let _ = cr.fill();
    }
}

fn draw_legend(
    cr: &cairo::Context,
    layout: &pango::Layout,
    d: &Data,
    width: f64,
    fg: gdk::RGBA,
    text_h: f64,
) -> f64 {
    let swatch = (text_h * 0.6).round();
    let row_h = text_h + 4.0;
    let (mut x, mut y) = (4.0, 0.0);
    for s in &d.series {
        layout.set_text(&s.label);
        let lw = layout.pixel_size().0 as f64;
        let item_w = swatch + 5.0 + lw + 14.0;
        if x > 4.0 && x + item_w > width {
            x = 4.0;
            y += row_h;
        }
        set_color(cr, &palette::resolve(s.color, d.palette.as_ref(), fg));
        rounded_rect(cr, x, y + (text_h - swatch) / 2.0, swatch, swatch, 2.0);
        let _ = cr.fill();
        set_color(cr, &with_alpha(fg, 0.75));
        cr.move_to(x + swatch + 5.0, y);
        pangocairo::functions::show_layout(cr, layout);
        x += item_w;
    }
    y + row_h + 2.0
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

fn local_offset() -> i64 {
    glib::DateTime::now_local()
        .map(|t| t.utc_offset().as_seconds())
        .unwrap_or(0)
}

fn format_local(t: i64, fmt: &str) -> String {
    glib::DateTime::from_unix_local(t)
        .ok()
        .and_then(|dt| dt.format(fmt).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn draw_time_axis(
    cr: &cairo::Context,
    layout: &pango::Layout,
    d: &Data,
    fg: gdk::RGBA,
    bottom: f64,
) {
    let p = d.plot;
    let span = d.to - d.from;
    let step = axis::time_step(span, (p.width / 90.0) as i64);
    let offset = local_offset();
    set_color(cr, &with_alpha(fg, LABEL_ALPHA));
    let mut last_right = f64::NEG_INFINITY;
    for t in axis::time_ticks(d.from, d.to, step, offset) {
        let midnight = (t + offset).rem_euclid(86400) == 0;
        let label = if step >= 86400 || (midnight && span > 86400) {
            format_local(t, "%a %-e")
        } else {
            format_local(t, "%H:%M")
        };
        layout.set_text(&label);
        let lw = layout.pixel_size().0 as f64;
        let x = (d.x_of(t as f64) - lw / 2.0).clamp(0.0, p.left + p.width - lw);
        if x < last_right + 8.0 {
            continue;
        }
        cr.move_to(x, bottom + 4.0);
        pangocairo::functions::show_layout(cr, layout);
        last_right = x + lw;
    }
}

fn segments(d: &Data, s: &Series, y_of: &impl Fn(f64) -> f64) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut prev_t: Option<i64> = None;
    let half = d.bucket as f64 / 2.0;
    for &(t, v) in &s.points {
        let gap =
            prev_t.is_some_and(|pt| t - pt > 3 * d.bucket || d.breaks.binary_search(&t).is_ok());
        if (v.is_none() || gap) && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if let Some(v) = v {
            current.push((d.x_of(t as f64 + half), y_of(v)));
        }
        prev_t = Some(t);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn draw_series(
    cr: &cairo::Context,
    d: &Data,
    s: &Series,
    color: gdk::RGBA,
    line_width: f64,
    y_of: &impl Fn(f64) -> f64,
    bottom: f64,
) {
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_line_cap(cairo::LineCap::Round);
    for seg in segments(d, s, y_of) {
        let path = |cr: &cairo::Context| {
            if seg.len() == 1 {
                cr.move_to(seg[0].0 - 1.0, seg[0].1);
                cr.line_to(seg[0].0 + 1.0, seg[0].1);
            } else {
                cr.move_to(seg[0].0, seg[0].1);
                for &(x, y) in &seg[1..] {
                    cr.line_to(x, y);
                }
            }
        };

        if s.style == Style::Fill {
            path(cr);
            let (first, last) = (seg[0].0, seg[seg.len() - 1].0);
            cr.line_to(if seg.len() == 1 { first + 1.0 } else { last }, bottom);
            cr.line_to(if seg.len() == 1 { first - 1.0 } else { first }, bottom);
            cr.close_path();
            let gradient = cairo::LinearGradient::new(0.0, d.plot.top, 0.0, bottom);
            let (r, g, b) = (
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
            );
            gradient.add_color_stop_rgba(0.0, r, g, b, FILL_TOP_ALPHA);
            gradient.add_color_stop_rgba(1.0, r, g, b, FILL_BOTTOM_ALPHA);
            let _ = cr.set_source(&gradient);
            let _ = cr.fill();
        }

        path(cr);
        set_color(cr, &color);
        match s.style {
            Style::Dashed => {
                cr.set_line_width(1.0);
                cr.set_dash(&[4.0, 4.0], 0.0);
            }
            Style::Line => cr.set_line_width(line_width * 0.75),
            Style::Fill => cr.set_line_width(line_width),
        }
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
    }
}

fn value_at(s: &Series, t: i64, bucket: i64) -> Option<f64> {
    let i = s.points.partition_point(|p| p.0 <= t);
    let (pt, v) = *s.points.get(i.checked_sub(1)?)?;
    (t - pt < bucket).then_some(v).flatten()
}

fn tooltip_markup(d: &Data, t: i64) -> String {
    let t = t.div_euclid(d.bucket) * d.bucket;
    let fmt = if d.to - d.from > 86400 {
        "%a %-e %b, %H:%M:%S"
    } else {
        "%H:%M:%S"
    };
    let mut out = format!("<b>{}</b>", glib::markup_escape_text(&format_local(t, fmt)));
    for s in &d.series {
        let value = value_at(s, t, d.bucket).map_or("–".to_string(), |v| d.format.format(v));
        out.push_str(&format!(
            "\n{}: {}",
            glib::markup_escape_text(&s.label),
            glib::markup_escape_text(&value)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(bucket: i64, breaks: &[i64]) -> Data {
        Data {
            from: 0,
            to: 1000,
            bucket,
            series: Vec::new(),
            breaks: breaks.into(),
            y_range: YRange::Auto,
            format: Format::Percent,
            plot: Plot {
                left: 0.0,
                top: 0.0,
                width: 1000.0,
                height: 100.0,
            },
            hover_x: None,
            drag: None,
            on_zoom: None,
            palette: None,
        }
    }

    fn lengths(d: &Data, points: Vec<(i64, Option<f64>)>) -> Vec<usize> {
        let s = Series {
            label: String::new(),
            points,
            style: Style::Line,
            color: ColorRole::Accent,
        };
        segments(d, &s, &|v| v).iter().map(Vec::len).collect()
    }

    #[test]
    fn lines_break_at_nulls_wide_gaps_and_marked_buckets() {
        let pts = |ts: &[i64]| ts.iter().map(|&t| (t, Some(1.0))).collect::<Vec<_>>();
        assert_eq!(lengths(&data(10, &[]), pts(&[0, 10, 20, 30])), vec![4]);
        assert_eq!(lengths(&data(10, &[]), pts(&[0, 10, 50, 60])), vec![2, 2]);
        assert_eq!(lengths(&data(10, &[20]), pts(&[0, 10, 20, 30])), vec![2, 2]);
        assert_eq!(
            lengths(
                &data(10, &[]),
                vec![(0, Some(1.0)), (10, None), (20, Some(1.0))]
            ),
            vec![1, 1]
        );
    }
}
