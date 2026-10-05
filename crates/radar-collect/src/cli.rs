use std::path::PathBuf;

pub enum Mode {
    Run,
    Once,
    ListSensors,
    SeedDemo(u64),
    Serve,
}

pub struct Args {
    pub db: PathBuf,
    pub interval: u64,
    pub retention_days: u64,
    pub root: PathBuf,
    pub mode: Mode,
}

const USAGE: &str = "\
Usage: radar-collect [OPTIONS]

Options:
  --db <path>          database path (default: $XDG_DATA_HOME/radar/radar.db)
  --interval <secs>    sample interval (default: 5)
  --retention <days>   days of history to keep (default: 5)
  --once               take two samples, print them, write nothing
  --list-sensors       print discovered sensors, interfaces and disks
  --seed-demo <days>   fill the database with synthetic history
  --serve              answer the viewer's queries on stdin and stdout
  --root <path>        filesystem root for /proc and /sys (default: /)
  -h, --help           show this help";

pub fn parse() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;

    let mut args = Args {
        db: radar_core::db::default_db_path(),
        interval: 5,
        retention_days: 5,
        root: PathBuf::from("/"),
        mode: Mode::Run,
    };
    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Long("db") => args.db = parser.value()?.into(),
            Long("interval") => args.interval = parser.value()?.parse::<u64>()?.max(1),
            Long("retention") => args.retention_days = parser.value()?.parse::<u64>()?.max(1),
            Long("root") => args.root = parser.value()?.into(),
            Long("once") => args.mode = Mode::Once,
            Long("list-sensors") => args.mode = Mode::ListSensors,
            Long("seed-demo") => args.mode = Mode::SeedDemo(parser.value()?.parse()?),
            Long("serve") => args.mode = Mode::Serve,
            Short('h') | Long("help") => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}
