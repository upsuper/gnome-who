use anyhow::{Context, Error, Result};
use glib::{MainContext, Sender};
use gtk::{
    ButtonsType, CheckMenuItem, CheckMenuItemExt, DialogFlags, GtkMenuItemExt, Menu, MenuItem,
    MenuShellExt, MessageDialog, MessageType, SeparatorMenuItem, WidgetExt, Window,
};
use inotify::{Inotify, WatchMask};
use libappindicator::{AppIndicator, AppIndicatorStatus};
use once_cell::sync::Lazy;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::thread;
use tempfile::TempDir;
use utmp_rs::UtmpEntry;

const UTMP_PATH: &str = "/var/run/utmp";
const NORMAL_ICON: &[u8] = include_bytes!("../icons/normal.svg");
const WARNING_ICON: &[u8] = include_bytes!("../icons/warning.svg");

static DISPLAY: Lazy<String> = Lazy::new(|| env::var("DISPLAY").expect("no DISPLAY specified"));

enum Message {
    Update(Vec<UtmpEntry>),
    Error(Error),
}

fn main() -> Result<()> {
    gtk::init().context("failed to init GTK")?;

    let (tx, rx) = MainContext::channel(glib::PRIORITY_HIGH);
    thread::spawn(move || {
        // Ignore if sending failed, because the receiver may have died.
        let _ = tx.send(Message::Error(match watch_utmp(tx.clone()) {
            Ok(_) => unreachable!(),
            Err(e) => e,
        }));
    });

    let temp_dir = TempDir::new().context("failed to create temp dir")?;
    let temp_path = temp_dir.path();
    fs::write(temp_path.join("normal.svg"), NORMAL_ICON)?;
    fs::write(temp_path.join("warning.svg"), WARNING_ICON)?;

    let mut indicator = AppIndicator::new("who", "normal");
    indicator.set_icon_theme_path(temp_path.to_str().unwrap());
    indicator.set_status(AppIndicatorStatus::Active);

    rx.attach(None, move |msg| match msg {
        Message::Update(entries) => {
            update_indicator(&mut indicator, &entries);
            glib::Continue(true)
        }
        Message::Error(e) => {
            let message = format!("{}", e);
            let dialog = MessageDialog::new::<Window>(
                None,
                DialogFlags::MODAL,
                MessageType::Error,
                ButtonsType::Ok,
                &message,
            );
            dialog.show_all();
            glib::Continue(false)
        }
    });

    gtk::main();
    Ok(())
}

fn watch_utmp(tx: Sender<Message>) -> Result<()> {
    let mut inotify = Inotify::init().context("failed to init inotify")?;
    inotify
        .add_watch(UTMP_PATH, WatchMask::CLOSE_WRITE)
        .context("failed to watch utmp file")?;
    notify_utmp_update(&tx)?;
    let mut buffer = [0; 1024];
    loop {
        inotify
            .read_events_blocking(&mut buffer)
            .context("failed to wait for events")?;
        notify_utmp_update(&tx)?;
    }
}

fn notify_utmp_update(tx: &Sender<Message>) -> Result<()> {
    let utmp_entries = utmp_rs::parse_from_path(UTMP_PATH)?;
    tx.send(Message::Update(utmp_entries))?;
    Ok(())
}

fn update_indicator(indicator: &mut AppIndicator, utmp_entries: &[UtmpEntry]) {
    let mut menu = Menu::new();
    let mut count = 0;
    for entry in utmp_entries {
        if let UtmpEntry::UserProcess {
            user,
            line,
            host,
            time,
            ..
        } = entry
        {
            let mut label = format!("{} - {} / {}", time.format("%Y-%m-%d %H:%M:%S"), user, line);
            if !host.is_empty() {
                write!(&mut label, " @ {}", host).unwrap();
            }
            if line == &*DISPLAY {
                let item = CheckMenuItem::new_with_label(&label);
                let is_current = line == &*DISPLAY;
                item.set_active(is_current);
                item.set_sensitive(!is_current);
                item.set_draw_as_radio(true);
                menu.append(&item);
            } else {
                let item = MenuItem::new_with_label(&label);
                menu.append(&item);
            }
            count += 1;
        }
    }
    menu.append(&SeparatorMenuItem::new());
    let quit_item = MenuItem::new_with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    menu.append(&quit_item);

    indicator.set_menu(&mut menu);
    menu.show_all();

    let icon = if count > 1 { "warning" } else { "normal" };
    indicator.set_icon_full(icon, icon);
}
