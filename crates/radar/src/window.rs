use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc;

use adw::prelude::*;
use gtk::glib;
use omarchy_theme::Theme;
use radar_core::{Sensor, SensorKind, Stats, SysPoint, query};

use crate::axis::Format;
use crate::cards::Card;
use crate::chart::{Series, Style, YRange};
use crate::config::Config;
use crate::palette::{CYCLE, ColorRole, Hue};
use crate::procs::ProcPanel;
use crate::range::{Range, RangePicker};
use crate::worker::{self, Request, Snapshot};

const REFRESH_SECS: u32 = 5;
const TOP_N: i64 = 10;
const DEFAULT_WIDTH_PX: i32 = 800;

struct Cards {
    cpu: Card,
    temps: Card,
    mem: Card,
    net: Card,
    disk: Card,
    gpu: Card,
    fans: Card,
    power: Card,
}

impl Cards {
    fn all(&self) -> [&Card; 8] {
        [
            &self.cpu,
            &self.temps,
            &self.mem,
            &self.net,
            &self.disk,
            &self.gpu,
            &self.fans,
            &self.power,
        ]
    }
}

struct Viewer {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    status: adw::StatusPage,
    picker: RangePicker,
    charts_column: gtk::Box,
    cards: Cards,
    sensor_menu: gtk::Box,
    procs: ProcPanel,
    range: Cell<Range>,
    generation: Cell<u64>,
    requests: mpsc::Sender<Request>,
    last: RefCell<Option<Snapshot>>,
    config: RefCell<Config>,
    menu_sensors: RefCell<Vec<String>>,
    theme: Theme,
    this: Weak<Viewer>,
}

pub fn build(app: &adw::Application, db: PathBuf, preset_secs: i64, theme: Theme) {
    let (requests, replies) = worker::spawn(db.clone());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Radar")
        .default_width(1280)
        .default_height(900)
        .width_request(360)
        .height_request(400)
        .build();

    let viewer: Rc<Viewer> = Rc::new_cyclic(|this: &Weak<Viewer>| {
        let weak = this.clone();
        let picker = RangePicker::new(move |range| {
            if let Some(v) = weak.upgrade() {
                v.set_range(range);
            }
        });
        let cards = Cards {
            cpu: Card::new("CPU", Format::Percent, YRange::Fixed(100.0)),
            temps: Card::new("Temperatures", Format::Celsius, YRange::Auto),
            mem: Card::new("Memory", Format::Bytes, YRange::Auto),
            net: Card::new("Network", Format::BytesPerSec, YRange::Auto),
            disk: Card::new("Disk", Format::BytesPerSec, YRange::Auto),
            gpu: Card::new("GPU", Format::Percent, YRange::Fixed(100.0)),
            fans: Card::new("Fans", Format::Rpm, YRange::Auto),
            power: Card::new("Battery Power", Format::Watts, YRange::Auto),
        };
        let status = adw::StatusPage::builder()
            .icon_name("dev.radar.Radar")
            .title("No Data Yet")
            .build();
        Viewer {
            window: window.clone(),
            stack: gtk::Stack::new(),
            status,
            picker,
            charts_column: gtk::Box::new(gtk::Orientation::Vertical, 12),
            cards,
            sensor_menu: gtk::Box::new(gtk::Orientation::Vertical, 2),
            procs: ProcPanel::new(),
            range: Cell::new(Range::Preset(preset_secs)),
            generation: Cell::new(0),
            requests,
            last: RefCell::new(None),
            config: RefCell::new(Config::load()),
            menu_sensors: RefCell::new(Vec::new()),
            theme,
            this: this.clone(),
        }
    });

    viewer.layout();
    viewer.connect_theme();
    for card in viewer.cards.all() {
        let weak = Rc::downgrade(&viewer);
        card.chart.connect_zoom(move |from, to| {
            if let Some(v) = weak.upgrade() {
                v.set_range(Range::Custom { from, to });
            }
        });
    }

    let weak = Rc::downgrade(&viewer);
    glib::spawn_future_local(async move {
        while let Ok(reply) = replies.recv().await {
            let Some(v) = weak.upgrade() else { break };
            v.receive(reply);
        }
    });

    let weak = Rc::downgrade(&viewer);
    glib::timeout_add_seconds_local(REFRESH_SECS, move || {
        let Some(v) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if matches!(v.range.get(), Range::Preset(_)) {
            v.request();
        }
        glib::ControlFlow::Continue
    });

    viewer.set_range(Range::Preset(preset_secs));
    window.present();

    let owner = RefCell::new(Some(viewer));
    window.connect_destroy(move |_| drop(owner.take()));
}

fn now() -> i64 {
    glib::DateTime::now_utc().map(|d| d.to_unix()).unwrap_or(0)
}

impl Viewer {
    fn layout(self: &Rc<Self>) {
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&self.picker.root));

        for card in self.cards.all() {
            self.charts_column.append(&card.root);
        }
        for card in [&self.cards.gpu, &self.cards.fans, &self.cards.power] {
            card.root.set_visible(false);
        }

        let sensor_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Choose Sensors")
            .valign(gtk::Align::Center)
            .build();
        sensor_button.add_css_class("flat");
        let popover_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(480)
            .child(&self.sensor_menu)
            .build();
        sensor_button.set_popover(Some(
            &gtk::Popover::builder().child(&popover_scroll).build(),
        ));
        self.cards.temps.header.append(&sensor_button);

        let clamp = adw::Clamp::builder()
            .maximum_size(1600)
            .tightening_threshold(1200)
            .child(&self.charts_column)
            .build();
        let margins = |w: &gtk::Widget, m: i32| {
            w.set_margin_top(m);
            w.set_margin_bottom(m);
            w.set_margin_start(m);
            w.set_margin_end(m);
        };
        margins(clamp.upcast_ref(), 12);
        let charts_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .child(&clamp)
            .build();

        margins(self.procs.root.upcast_ref(), 12);
        let side = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(340)
            .child(&self.procs.root)
            .build();
        let side_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        side_box.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        side_box.append(&side);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&charts_scroll);
        body.append(&side_box);

        self.stack.add_named(&body, Some("data"));
        self.stack.add_named(&self.status, Some("status"));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&self.stack));
        self.window.set_content(Some(&toolbar));

        let breakpoint =
            adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 960sp").unwrap());
        {
            let (procs, column, side, side_box) = (
                self.procs.root.clone(),
                self.charts_column.clone(),
                side.clone(),
                side_box.clone(),
            );
            breakpoint.connect_apply(move |_| {
                side.set_child(None::<&gtk::Widget>);
                procs.set_margin_start(0);
                procs.set_margin_end(0);
                column.append(&procs);
                side_box.set_visible(false);
            });
        }
        {
            let (procs, column, side, side_box) = (
                self.procs.root.clone(),
                self.charts_column.clone(),
                side.clone(),
                side_box.clone(),
            );
            breakpoint.connect_unapply(move |_| {
                column.remove(&procs);
                procs.set_margin_start(12);
                procs.set_margin_end(12);
                side.set_child(Some(&procs));
                side_box.set_visible(true);
            });
        }
        self.window.add_breakpoint(breakpoint);
    }

    fn connect_theme(self: &Rc<Self>) {
        let sm = adw::StyleManager::default();
        let redraw = {
            let weak = Rc::downgrade(self);
            move |_: &adw::StyleManager| {
                if let Some(v) = weak.upgrade() {
                    v.cards.all().iter().for_each(|c| c.chart.queue_draw());
                }
            }
        };
        sm.connect_dark_notify(redraw.clone());
        sm.connect_accent_color_notify(redraw.clone());
        sm.connect_high_contrast_notify(redraw);

        self.set_palette(self.theme.palette());
        let weak = Rc::downgrade(self);
        self.theme.connect_changed(move |palette| {
            if let Some(v) = weak.upgrade() {
                v.set_palette(palette.cloned());
            }
        });
    }

    fn set_palette(&self, palette: Option<omarchy_theme::Palette>) {
        for card in self.cards.all() {
            card.chart.set_palette(palette.clone());
        }
    }

    fn set_range(&self, range: Range) {
        self.range.set(range);
        self.picker.show(range);
        self.request();
    }

    fn request(&self) {
        let (from, to) = self.range.get().bounds(now());
        let width = self.cards.cpu.chart.widget().width();
        let width = if width > 0 { width } else { DEFAULT_WIDTH_PX };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let bucket = query::nice_bucket(to - from, width);
        let _ = self.requests.send(Request {
            generation,
            from,
            to,
            bucket,
            top_n: TOP_N,
        });
    }

    fn receive(&self, reply: worker::Reply) {
        match reply {
            Ok(snap) if snap.generation == self.generation.get() => {
                self.stack.set_visible_child_name("data");
                self.picker.set_oldest(snap.oldest);
                self.render(&snap);
                *self.last.borrow_mut() = Some(snap);
            }
            Err((generation, message)) if generation == self.generation.get() => {
                self.status.set_description(Some(&format!(
                    "{message}\n\nStart the collector with\n<tt>systemctl --user enable --now radar-collect</tt>"
                )));
                self.stack.set_visible_child_name("status");
            }
            _ => {}
        }
    }

    fn render(&self, snap: &Snapshot) {
        let c = &self.cards;
        let sys = |f: fn(&SysPoint) -> Option<f64>| -> Vec<(i64, Option<f64>)> {
            snap.sys.iter().map(|p| (p.t, f(p))).collect()
        };
        let breaks: Rc<[i64]> = snap.sys.iter().filter(|p| p.gap).map(|p| p.t).collect();
        let set = |card: &Card, series: Vec<Series>, stats: Option<Stats>| {
            card.set_summary(series.first().map_or(&[][..], |s| &s.points), stats);
            card.chart
                .set_data(snap.from, snap.to, snap.bucket, series, breaks.clone());
        };
        let line = |label: &str, points, style, color| Series {
            label: label.into(),
            points,
            style,
            color,
        };

        set(
            &c.cpu,
            vec![
                line("Busy", sys(|p| p.cpu_busy), Style::Fill, ColorRole::Accent),
                line(
                    "I/O wait",
                    sys(|p| p.cpu_iowait),
                    Style::Line,
                    ColorRole::Named(Hue::Orange),
                ),
            ],
            snap.sys_stats.cpu_busy,
        );

        let mut mem = vec![
            line("Used", sys(|p| p.mem_used), Style::Fill, ColorRole::Accent),
            line(
                "Total",
                sys(|p| p.mem_total),
                Style::Dashed,
                ColorRole::Foreground,
            ),
        ];
        if snap.sys.iter().any(|p| p.swap_used.unwrap_or(0.0) > 0.0) {
            mem.push(line(
                "Swap",
                sys(|p| p.swap_used),
                Style::Line,
                ColorRole::Named(Hue::Purple),
            ));
        }
        set(&c.mem, mem, snap.sys_stats.mem_used);

        set(
            &c.net,
            vec![
                line(
                    "Received",
                    sys(|p| p.net_rx),
                    Style::Fill,
                    ColorRole::Named(Hue::Blue),
                ),
                line(
                    "Sent",
                    sys(|p| p.net_tx),
                    Style::Fill,
                    ColorRole::Named(Hue::Green),
                ),
            ],
            snap.sys_stats.net_rx,
        );
        set(
            &c.disk,
            vec![
                line(
                    "Read",
                    sys(|p| p.disk_read),
                    Style::Fill,
                    ColorRole::Named(Hue::Purple),
                ),
                line(
                    "Write",
                    sys(|p| p.disk_write),
                    Style::Fill,
                    ColorRole::Named(Hue::Orange),
                ),
            ],
            snap.sys_stats.disk_read,
        );

        let mut by_sensor: HashMap<i64, Vec<(i64, Option<f64>)>> = HashMap::new();
        for p in &snap.sensor_points {
            by_sensor
                .entry(p.sensor_id)
                .or_default()
                .push((p.t, Some(p.value)));
        }
        let config = self.config.borrow();
        let sensor_series =
            |kind: SensorKind, filter: &dyn Fn(&Sensor) -> bool| -> (Vec<Series>, Option<Stats>) {
                let shown: Vec<(usize, &Sensor)> = snap
                    .sensors
                    .iter()
                    .filter(|s| s.kind == kind)
                    .enumerate()
                    .filter(|(_, s)| filter(s))
                    .collect();
                let stats = shown
                    .first()
                    .and_then(|(_, s)| snap.sensor_stats.get(&s.id).copied());
                let series = shown
                    .into_iter()
                    .map(|(i, s)| {
                        let color = if kind == SensorKind::Temp {
                            ColorRole::Named(CYCLE[i % CYCLE.len()])
                        } else if i == 0 {
                            ColorRole::Accent
                        } else {
                            ColorRole::Named(CYCLE[i % CYCLE.len()])
                        };
                        let points = by_sensor.get(&s.id).cloned().unwrap_or_default();
                        let style = if kind == SensorKind::Temp {
                            Style::Line
                        } else {
                            Style::Fill
                        };
                        line(&sensor_label(s, &snap.sensors), points, style, color)
                    })
                    .collect();
                (series, stats)
            };
        let (series, stats) = sensor_series(SensorKind::Temp, &|s| config.visible(s));
        set(&c.temps, series, stats);
        for (card, kind) in [
            (&c.gpu, SensorKind::GpuBusy),
            (&c.fans, SensorKind::Fan),
            (&c.power, SensorKind::Power),
        ] {
            let (series, stats) = sensor_series(kind, &|_| true);
            card.root.set_visible(!series.is_empty());
            set(card, series, stats);
        }
        drop(config);

        self.update_sensor_menu(&snap.sensors);
        let (lo, hi) = query::proc_span(snap.from, snap.to);
        self.procs.set(&snap.procs, &snap.meta, hi - lo);
    }

    fn update_sensor_menu(&self, sensors: &[Sensor]) {
        let temps: Vec<&Sensor> = sensors
            .iter()
            .filter(|s| s.kind == SensorKind::Temp)
            .collect();
        let keys: Vec<String> = temps.iter().map(|s| s.key()).collect();
        if *self.menu_sensors.borrow() == keys {
            return;
        }
        while let Some(child) = self.sensor_menu.first_child() {
            self.sensor_menu.remove(&child);
        }
        for s in temps {
            let check = gtk::CheckButton::with_label(&sensor_label(s, sensors));
            check.set_active(self.config.borrow().visible(s));
            let (sensor, viewer) = (s.clone(), self.this.clone());
            check.connect_toggled(move |b| {
                let Some(v) = viewer.upgrade() else { return };
                {
                    let mut config = v.config.borrow_mut();
                    config.set_visible(&sensor, b.is_active());
                    config.save();
                }
                if let Some(snap) = v.last.borrow().as_ref() {
                    v.render(snap);
                }
            });
            self.sensor_menu.append(&check);
        }
        *self.menu_sensors.borrow_mut() = keys;
    }
}

fn base_chip(chip: &str) -> &str {
    chip.split_once(':').map_or(chip, |(base, _)| base)
}

/// Chips are stored with their device; it is only shown when needed to tell chips apart.
fn sensor_label(s: &Sensor, all: &[Sensor]) -> String {
    let base = base_chip(&s.chip);
    let ambiguous = all
        .iter()
        .any(|o| o.kind == s.kind && o.chip != s.chip && base_chip(&o.chip) == base);
    match s.chip.split_once(':') {
        Some((_, device)) if ambiguous && device.starts_with(base) => {
            format!("{device} {}", s.label)
        }
        Some((_, device)) if ambiguous => format!("{base} {device} {}", s.label),
        _ => format!("{base} {}", s.label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(chip: &str, label: &str) -> Sensor {
        Sensor {
            id: 0,
            kind: SensorKind::Temp,
            chip: chip.into(),
            label: label.into(),
            unit: "°C".into(),
        }
    }

    #[test]
    fn labels_show_device_only_when_ambiguous() {
        let all = [
            temp("k10temp:0000:00:18.3", "Tctl"),
            temp("nvme:nvme0", "Composite"),
            temp("nvme:nvme1", "Composite"),
            temp("spd5118:8-0051", "temp1"),
            temp("spd5118:8-0053", "temp1"),
            temp("amdgpu:0000:03:00.0", "edge"),
            Sensor {
                kind: SensorKind::GpuBusy,
                ..temp("amdgpu", "card1")
            },
        ];
        assert_eq!(sensor_label(&all[5], &all), "amdgpu edge");
        assert_eq!(sensor_label(&all[0], &all), "k10temp Tctl");
        assert_eq!(sensor_label(&all[1], &all), "nvme0 Composite");
        assert_eq!(sensor_label(&all[3], &all), "spd5118 8-0051 temp1");
        assert_eq!(sensor_label(&all[1], &all[..2]), "nvme Composite");
    }
}
