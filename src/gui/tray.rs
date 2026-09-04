//! The tray (Windows) / menu bar (macOS) icon.
//!
//! A router that stops when its window is closed is not a router, so on
//! the two platforms where a tray is a first-class thing, closing the
//! window hides it and the node, the proxies and the polls carry on; the
//! tray menu brings the window back or quits for real. Linux desktops
//! disagree with each other about trays (and the crate that draws one
//! needs GTK and an appindicator library at build time), so there the
//! window closes as any window does and a `run --router` service is the
//! way to keep the router up.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// What the operator asked for through the tray.
#[derive(Debug, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Quit,
}

pub struct Tray {
    // Dropping it removes the icon; held for the life of the window.
    _icon: TrayIcon,
    open: MenuId,
    quit: MenuId,
    events: Mutex<Receiver<Event>>,
}

enum Event {
    Menu(MenuId),
    Click,
}

impl Tray {
    /// Must run on the thread that pumps the window's event loop (the
    /// creator closure eframe calls does), and after that loop exists.
    pub fn build(ctx: &egui::Context, title: &str) -> Result<Self, String> {
        let icon = Icon::from_rgba(super::icon::rgba(32), 32, 32).map_err(|e| e.to_string())?;
        let menu = Menu::new();
        let open = MenuItem::new("Open KMPLIFY Node", true, None);
        let quit = MenuItem::new("Quit KMPLIFY Node", true, None);
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(title)
            .with_icon(icon)
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;

        // The crate delivers events to a handler INSTEAD of its channel once
        // one is set; the handler forwards to this tray's own channel and
        // wakes the window, which otherwise repaints nothing while hidden.
        let (tx, rx) = channel::<Event>();
        let tx: Mutex<Sender<Event>> = Mutex::new(tx);
        let (menu_tx, click_tx) = {
            let a = tx.lock().map_err(|e| e.to_string())?.clone();
            let b = a.clone();
            (Mutex::new(a), Mutex::new(b))
        };
        let c = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Ok(tx) = menu_tx.lock() {
                let _ = tx.send(Event::Menu(e.id().clone()));
            }
            c.request_repaint();
        }));
        let c = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                if let Ok(tx) = click_tx.lock() {
                    let _ = tx.send(Event::Click);
                }
            }
            c.request_repaint();
        }));

        Ok(Self {
            _icon: tray,
            open: open.id().clone(),
            quit: quit.id().clone(),
            events: Mutex::new(rx),
        })
    }

    /// Whatever arrived since the last frame, last order wins.
    pub fn poll(&self) -> Option<TrayAction> {
        let rx = self.events.lock().ok()?;
        let mut action = None;
        while let Ok(e) = rx.try_recv() {
            action = match e {
                Event::Click => Some(TrayAction::Open),
                Event::Menu(id) if id == self.open => Some(TrayAction::Open),
                Event::Menu(id) if id == self.quit => Some(TrayAction::Quit),
                Event::Menu(_) => action,
            };
        }
        action
    }
}
