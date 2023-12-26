use anyhow::{Context, Error, Result};
use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use glib::MainContext;
use gtk::prelude::*;
use gtk::{
    ButtonsType, CheckMenuItem, DialogFlags, Menu, MenuItem, MessageDialog, MessageType,
    SeparatorMenuItem, Window,
};
use inotify::{Inotify, WatchMask};
use libappindicator::{AppIndicator, AppIndicatorStatus};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use mio_pidfd::PidFd;
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::thread;
use tempfile::TempDir;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::UtcOffset;
use utmp_rs::UtmpEntry;

const UTMP_PATH: &str = "/var/run/utmp";
const NORMAL_ICON: &[u8] = include_bytes!("../icons/normal.svg");
const WARNING_ICON: &[u8] = include_bytes!("../icons/warning.svg");

const IGNORED_HOSTS: &[&str] = &["login screen"];

static DISPLAY: Lazy<String> = Lazy::new(|| env::var("DISPLAY").expect("no DISPLAY specified"));

enum Message {
    Update(Vec<Entry>),
    Error(Error),
}

struct Entry {
    pid: Pid,
    label: String,
    is_current: bool,
    should_ignore: bool,
    can_kill: bool,
}

fn main() -> Result<()> {
    gtk::init().context("failed to init GTK")?;

    let (tx, rx) = futures_channel::mpsc::unbounded();
    thread::spawn(move || {
        let result = watch_entries(|entries| {
            let _ = tx.unbounded_send(Message::Update(entries));
        });
        match result {
            Ok(()) => unreachable!(),
            Err(e) => {
                // Ignore if sending failed, because the receiver may have died.
                let _ = tx.unbounded_send(Message::Error(e));
            }
        };
    });

    let temp_dir = TempDir::new().context("failed to create temp dir")?;
    let temp_path = temp_dir.path();
    fs::write(temp_path.join("normal.svg"), NORMAL_ICON)?;
    fs::write(temp_path.join("warning.svg"), WARNING_ICON)?;

    let mut indicator = AppIndicator::new("who", "normal");
    indicator.set_icon_theme_path(temp_path.to_str().unwrap());
    indicator.set_status(AppIndicatorStatus::Active);

    MainContext::default().spawn_local(handle_messages(indicator, rx));

    gtk::main();
    Ok(())
}

const LOCAL_TIME_FORMAT: &[FormatItem<'_>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
const GENERAL_TIME_FORMAT: &[FormatItem<'_>] = format_description!(
    "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour \
         sign:mandatory]:[offset_minute]:[offset_second]"
);

fn watch_entries(f: impl Fn(Vec<Entry>)) -> Result<()> {
    let mut poll = Poll::new().context("failed to create poll")?;

    let mut inotify = Inotify::init().context("failed to init inotify")?;
    inotify
        .watches()
        .add(UTMP_PATH, WatchMask::CLOSE_WRITE)
        .context("failed to watch utmp file")?;
    poll.registry().register(
        &mut SourceFd(&inotify.as_raw_fd()),
        Token(0),
        Interest::READABLE,
    )?;

    let mut events = Events::with_capacity(1024);
    let mut inotify_buffer = [0u8; 4096];
    let mut pid_map = HashMap::new();
    loop {
        // Generate all valid entries from utmp.
        let entries = utmp_rs::parse_from_path(UTMP_PATH)
            .context("failed to read utmp")?
            .into_iter()
            .filter_map(|entry| {
                if let UtmpEntry::UserProcess {
                    pid,
                    user,
                    line,
                    host,
                    time,
                    ..
                } = entry
                {
                    let pid = Pid::from_raw(pid);
                    let can_kill = match signal::kill(pid, None) {
                        // Skip processes no longer exist.
                        Err(Errno::ESRCH) => return None,
                        Err(Errno::EPERM) => false,
                        _ => true,
                    };
                    let offset = UtcOffset::local_offset_at(time).ok();
                    let time = match offset {
                        Some(offset) => time.to_offset(offset).format(LOCAL_TIME_FORMAT).unwrap(),
                        None => time.format(GENERAL_TIME_FORMAT).unwrap(),
                    };
                    let mut label = format!("{} - {} / {}", time, user, line);
                    if !host.is_empty() {
                        write!(&mut label, " @ {}", host).unwrap();
                    }
                    let is_current = line == *DISPLAY;
                    let should_ignore = IGNORED_HOSTS.iter().any(|s| host == *s);
                    Some(Entry {
                        pid,
                        label,
                        is_current,
                        should_ignore,
                        can_kill,
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let registry = poll.registry();
        let mut old_pid_map = mem::take(&mut pid_map);
        for Entry { pid, .. } in entries.iter() {
            if let Some((pid, fd)) = old_pid_map.remove_entry(pid) {
                pid_map.insert(pid, fd);
            } else {
                let mut fd = PidFd::open(pid.as_raw(), 0).context("failed to open pid fd")?;
                registry
                    .register(&mut fd, Token(pid.as_raw() as usize), Interest::READABLE)
                    .context("failed to register pid fd")?;
                pid_map.insert(*pid, fd);
            }
        }
        for (_, mut fd) in old_pid_map.into_iter() {
            registry
                .deregister(&mut fd)
                .context("failed to deregister")?;
        }

        f(entries);
        loop {
            match poll.poll(&mut events, None) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::new(e).context("failed to poll")),
            }
        }

        if events.iter().any(|e| e.token() == Token(0)) {
            // Drain the inotify events if it's pending.
            loop {
                let events = inotify
                    .read_events(&mut inotify_buffer)
                    .map(|iter| iter.count())
                    .or_else(|err| match err.kind() {
                        ErrorKind::WouldBlock => Ok(0),
                        _ => Err(err),
                    })
                    .context("failed to read inotify events")?;
                if events == 0 {
                    break;
                }
            }
        }
    }
}

async fn handle_messages(mut indicator: AppIndicator, mut rx: UnboundedReceiver<Message>) {
    while let Some(msg) = rx.next().await {
        match msg {
            Message::Update(entries) => {
                update_indicator(&mut indicator, entries);
            }
            Message::Error(e) => {
                let message = format!("{:?}", e);
                let dialog = MessageDialog::new::<Window>(
                    None,
                    DialogFlags::MODAL,
                    MessageType::Error,
                    ButtonsType::Ok,
                    &message,
                );
                dialog.connect_response(|_, _| gtk::main_quit());
                dialog.show_all();
                break;
            }
        }
    }
}

fn update_indicator(indicator: &mut AppIndicator, entries: Vec<Entry>) {
    let mut menu = Menu::new();
    let mut has_non_current = false;
    for Entry {
        pid,
        label,
        is_current,
        should_ignore,
        can_kill,
    } in entries.into_iter()
    {
        if is_current {
            let item = CheckMenuItem::with_label(&label);
            item.set_active(true);
            item.set_sensitive(false);
            item.set_draw_as_radio(true);
            menu.append(&item);
        } else {
            let item = MenuItem::with_label(&label);
            item.set_sensitive(can_kill);
            item.connect_activate(move |_| {
                let _ = signal::kill(pid, Signal::SIGKILL);
            });
            menu.append(&item);
            if !should_ignore {
                has_non_current = true;
            }
        }
    }
    menu.append(&SeparatorMenuItem::new());
    let quit_item = MenuItem::with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    menu.append(&quit_item);

    indicator.set_menu(&mut menu);
    menu.show_all();

    let icon = if has_non_current { "warning" } else { "normal" };
    indicator.set_icon_full(icon, icon);
}
