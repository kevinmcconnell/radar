//! The machine picker in the header. Type an ssh destination, or pick one of
//! the recent machines that drop down below it while it has focus.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::config;

const MAX_RECENT: usize = 6;
const THIS_COMPUTER: &str = "This computer";
const LOCAL_ICON: &str = "computer-symbolic";
const REMOTE_ICON: &str = "network-server-symbolic";

/// A machine shown before: the ssh destination it was reached by, and the
/// hostname it reported.
#[derive(Debug, PartialEq)]
pub struct Recent {
    pub machine: String,
    pub hostname: String,
}

impl Recent {
    fn parse(line: &str) -> Option<Self> {
        let (machine, hostname) = line.split_once('\t').unwrap_or((line, ""));
        let machine = machine.trim();
        if machine.is_empty() {
            None
        } else {
            Some(Recent {
                machine: machine.to_string(),
                hostname: hostname.trim().to_string(),
            })
        }
    }

    fn serialize(&self) -> String {
        format!("{}\t{}", self.machine, self.hostname)
    }

    fn title(&self) -> &str {
        if self.hostname.is_empty() {
            &self.machine
        } else {
            &self.hostname
        }
    }
}

fn recent_path() -> PathBuf {
    config::dir().join("machines")
}

/// The machines shown most recently, newest first.
pub fn recent() -> Vec<Recent> {
    parse_recent(&std::fs::read_to_string(recent_path()).unwrap_or_default())
}

pub fn remember(machine: &str, hostname: &str) {
    let path = recent_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let lines: Vec<String> = touch(recent(), machine, hostname)
        .iter()
        .map(Recent::serialize)
        .collect();
    if let Err(e) = std::fs::write(&path, lines.join("\n") + "\n") {
        eprintln!("radar: saving {}: {e}", path.display());
    }
}

fn parse_recent(text: &str) -> Vec<Recent> {
    text.lines()
        .filter_map(Recent::parse)
        .take(MAX_RECENT)
        .collect()
}

fn touch(mut machines: Vec<Recent>, machine: &str, hostname: &str) -> Vec<Recent> {
    machines.retain(|m| m.machine != machine);
    machines.insert(
        0,
        Recent {
            machine: machine.to_string(),
            hostname: hostname.to_string(),
        },
    );
    machines.truncate(MAX_RECENT);
    machines
}

pub struct MachinePicker {
    pub root: gtk::Box,
    entry: gtk::Entry,
    popover: gtk::Popover,
    list: gtk::ListBox,
    choices: RefCell<Vec<Option<String>>>,
    current: RefCell<Option<String>>,
    on_change: Box<dyn Fn(Option<String>)>,
}

impl MachinePicker {
    pub fn new(on_change: impl Fn(Option<String>) + 'static) -> Rc<Self> {
        let entry = gtk::Entry::builder()
            .placeholder_text(THIS_COMPUTER)
            .primary_icon_name(LOCAL_ICON)
            .tooltip_text("The machine to show, as an ssh destination like user@host")
            .width_chars(12)
            .max_width_chars(24)
            .build();
        let show = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Show Machine")
            .build();
        let root = gtk::Box::builder().css_classes(["linked"]).build();
        root.append(&entry);
        root.append(&show);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let popover = gtk::Popover::builder()
            .child(&list)
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .build();
        popover.set_parent(&entry);

        let picker = Rc::new(MachinePicker {
            root,
            entry,
            popover,
            list,
            choices: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            on_change: Box::new(on_change),
        });
        picker.connect(&show);
        picker
    }

    pub fn show(&self, machine: Option<&str>) {
        *self.current.borrow_mut() = machine.map(String::from);
        self.entry.set_text(machine.unwrap_or_default());
        let icon = if machine.is_some() {
            REMOTE_ICON
        } else {
            LOCAL_ICON
        };
        self.entry.set_primary_icon_name(Some(icon));
    }

    fn connect(self: &Rc<Self>, show: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.entry
            .connect_activate(move |_| with(&weak, |p| p.choose_typed()));
        let weak = Rc::downgrade(self);
        show.connect_clicked(move |_| with(&weak, |p| p.choose_typed()));

        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            with(&weak, |p| {
                let choice = p.choices.borrow()[row.index() as usize].clone();
                p.choose(choice);
            })
        });

        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| with(&weak, |p| p.show_recent()));
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| hide_recent_when_focus_leaves(&weak));
        self.entry.add_controller(focus);

        let list_focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        list_focus.connect_leave(move |_| hide_recent_when_focus_leaves(&weak));
        self.list.add_controller(list_focus);

        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::Escape => {
                    p.cancel();
                    glib::Propagation::Stop
                }
                gdk::Key::Down if p.popover.is_visible() => {
                    p.list.child_focus(gtk::DirectionType::Down);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.entry.add_controller(keys);

        let list_keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        list_keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                with(&weak, |p| p.cancel());
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.list.add_controller(list_keys);
    }

    fn choose_typed(&self) {
        let text = self.entry.text();
        let machine = Some(text.trim()).filter(|m| !m.is_empty());
        self.choose(machine.map(String::from));
    }

    fn choose(&self, machine: Option<String>) {
        self.popover.popdown();
        self.release_focus();
        (self.on_change)(machine);
    }

    fn cancel(&self) {
        let current = self.current.borrow().clone();
        self.entry.set_text(current.as_deref().unwrap_or_default());
        self.popover.popdown();
        self.release_focus();
    }

    fn release_focus(&self) {
        if let Some(root) = self.entry.root() {
            root.set_focus(None::<&gtk::Widget>);
        }
    }

    /// Popping up under a window the compositor has not shown yet stalls GDK,
    /// so this waits for the window to be active.
    fn show_recent(&self) {
        let machines = recent();
        let active = self
            .entry
            .root()
            .and_downcast::<gtk::Window>()
            .is_some_and(|w| w.is_active());
        if machines.is_empty() || !active {
            self.popover.popdown();
            return;
        }

        self.list.remove_all();
        self.add_row(None, &glib::host_name(), THIS_COMPUTER, LOCAL_ICON);
        for m in &machines {
            let subtitle = if m.title() == m.machine {
                ""
            } else {
                &m.machine
            };
            self.add_row(Some(&m.machine), m.title(), subtitle, REMOTE_ICON);
        }
        *self.choices.borrow_mut() = std::iter::once(None)
            .chain(machines.into_iter().map(|m| Some(m.machine)))
            .collect();
        self.popover.set_size_request(self.root.width(), -1);
        self.popover.popup();
    }

    fn add_row(&self, machine: Option<&str>, title: &str, subtitle: &str, icon: &str) {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        if machine == self.current.borrow().as_deref() {
            row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
        }
        self.list.append(&row);
    }
}

fn with(picker: &Weak<MachinePicker>, f: impl FnOnce(&MachinePicker)) {
    if let Some(p) = picker.upgrade() {
        f(&p);
    }
}

/// Focus may be moving between the entry and the drop-down, so decide once
/// the new focus is known.
fn hide_recent_when_focus_leaves(picker: &Weak<MachinePicker>) {
    let picker = picker.clone();
    glib::idle_add_local_once(move || {
        with(&picker, |p| {
            let focus = p.entry.root().and_then(|r| r.focus());
            let inside =
                focus.is_some_and(|f| f.is_ancestor(&p.entry) || f.is_ancestor(&p.popover));
            if !inside {
                p.popover.popdown();
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(machines: &[Recent]) -> Vec<&str> {
        machines.iter().map(|m| m.machine.as_str()).collect()
    }

    #[test]
    fn touch_moves_to_the_top_and_caps_the_list() {
        let mut machines = Vec::new();
        for i in 0..MAX_RECENT + 2 {
            machines = touch(machines, &format!("host{i}"), "");
        }
        assert_eq!(
            names(&machines),
            ["host7", "host6", "host5", "host4", "host3", "host2"]
        );

        let machines = touch(machines, "host4", "renamed");
        assert_eq!(
            names(&machines),
            ["host4", "host7", "host6", "host5", "host3", "host2"]
        );
        assert_eq!(machines[0].hostname, "renamed");
    }

    #[test]
    fn hostnames_round_trip_and_title_falls_back_to_the_destination() {
        let machines = touch(touch(Vec::new(), "build", ""), "me@10.0.0.2", "workstation");
        let text = machines
            .iter()
            .map(Recent::serialize)
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_recent(&text);
        assert_eq!(parsed, machines);
        assert_eq!(parsed[0].title(), "workstation");
        assert_eq!(parsed[1].title(), "build");
    }

    #[test]
    fn parse_drops_blank_lines_and_caps_the_list() {
        let text = "a\n\n  b  \t box \nc\nd\ne\nf\ng\n";
        let parsed = parse_recent(text);
        assert_eq!(names(&parsed), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(parsed[1].hostname, "box");
        assert!(parse_recent("").is_empty());
    }
}
