mod axis;
mod cards;
mod chart;
mod config;
mod palette;
mod procs;
mod range;
mod window;
mod worker;

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;

use crate::range::{PRESETS, preset_named};

const APP_ID: &str = "dev.radar.Radar";

const USAGE: &str = "Usage: radar [--db <path>] [--range 5m|10m|15m|30m|1h|6h|24h|3d|5d]";

fn parse_args() -> (PathBuf, i64) {
    let mut db = radar_core::db::default_db_path();
    let mut range = PRESETS[0].1;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => match args.next() {
                Some(p) => db = PathBuf::from(p),
                None => exit_usage("--db needs a path"),
            },
            "--range" => {
                let name = args.next().unwrap_or_default();
                match preset_named(&name) {
                    Some(secs) => range = secs,
                    None => exit_usage(&format!("unknown range {name:?}")),
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => exit_usage(&format!("unexpected argument {other}")),
        }
    }
    (db, range)
}

fn exit_usage(message: &str) -> ! {
    eprintln!("radar: {message}\n{USAGE}");
    std::process::exit(2);
}

fn main() -> glib::ExitCode {
    let (db, range) = parse_args();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| range::load_style());
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        window::build(app, db.clone(), range, omarchy_theme::follow());
    });
    app.run_with_args::<&str>(&[])
}
