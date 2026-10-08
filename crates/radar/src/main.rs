mod askpass;
mod axis;
mod cards;
mod chart;
mod cli;
mod config;
mod integrate;
mod machine;
mod palette;
mod procs;
mod range;
mod window;
mod worker;

use adw::prelude::*;
use clap::Parser;
use gtk::glib;

use crate::cli::{Cli, Command, Tool};
use crate::integrate::Action;

const APP_ID: &str = "dev.radar.Radar";

fn main() -> glib::ExitCode {
    if let Some(code) = askpass::answer_for_ssh() {
        return code;
    }
    let cli = Cli::parse();
    let view = match cli.command {
        None => cli.view,
        Some(Command::View(view)) => view,
        Some(Command::Configure { tool: Tool::Claude }) => {
            std::process::exit(integrate::claude(Action::Configure))
        }
        Some(Command::Remove { tool: Tool::Claude }) => {
            std::process::exit(integrate::claude(Action::Remove))
        }
    };
    let cli::View { db, range, machine } = view;
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| range::load_style());
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        window::build(
            app,
            db.clone(),
            machine.clone(),
            range,
            omarchy_theme::follow(),
        );
    });
    app.run_with_args::<&str>(&[])
}
