// No console window next to the GUI.
#![windows_subsystem = "windows"]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, PoisonError};

use anyhow::{Context, anyhow};
use eframe::egui;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winsafe::prelude::*;
use winsafe::{HEVENT, co};

use zthing_gui::app::{self, App, PreviewPane, Shared};
use zthing_gui::config::{self, Theme};
use zthing_gui::text::{self, Texts};
use zthing_gui::{http, preview};

/// Rendered from `assets/` by build.rs, the sizes must match the ones there.
const WINDOW_ICON_SIZE: u32 = 256;
const WINDOW_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window_icon.rgba"));
const TRAY_ICON_SIZE: u32 = 32;
const TRAY_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_icon.rgba"));
/// Signals the running window that a second start handed over a query.
/// `Local\` gives every logon session its own window.
const OPEN_EVENT: &str = r"Local\ZTHING-open";
/// Scheme of the URL protocol, as in `zthing:*.iso`.
const SCHEME: &str = "zthing:";

fn main() -> anyhow::Result<()> {
    let texts = text::system();
    let app_data = std::env::var_os("APPDATA").context("APPDATA is not set")?;
    let local_app_data = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let handoff = PathBuf::from(local_app_data).join(r"ZTHING\open.txt");
    // The URL protocol starts `zthing-gui.exe "zthing:<query>"`. Browsers
    // percent-encode the query, a plain `+` stays a plus there.
    let opened = std::env::args().nth(1).map(|arg| {
        let query = arg
            .get(..SCHEME.len())
            .filter(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
            .map_or(arg.as_str(), |_| &arg[SCHEME.len()..]);
        http::decode(&query.trim_start_matches("//").replace('+', "%2B"))
    });

    let open_event = HEVENT::CreateEvent(None, false, false, Some(OPEN_EVENT))?;
    if winsafe::GetLastError() == co::ERROR::ALREADY_EXISTS {
        // A window already runs. It shows itself and takes over a non-empty query.
        if let Some(dir) = handoff.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&handoff, opened.unwrap_or_default())?;
        open_event.SetEvent()?;
        return Ok(());
    }

    let (config, config_problem) =
        config::load(&PathBuf::from(app_data).join(r"ZTHING\config.toml"));
    // The service indexes every drive, so the chosen drives and excluded
    // folders become terms in front of each query. The trailing backslash
    // hides a folder's contents but not folders that merely start with the same name.
    let drives = config
        .drives
        .iter()
        .filter_map(|drive| drive.chars().next())
        .map(|drive| format!("path:\"{}:\\\"", drive.to_ascii_uppercase()))
        .collect::<Vec<_>>()
        .join("|");
    let exclusions: String = config
        .exclude
        .iter()
        .map(|folder| format!("!path:\"{}\\\" ", folder.trim_end_matches('\\')))
        .collect();
    let prefix = if drives.is_empty() {
        exclusions
    } else {
        format!("{drives} {exclusions}")
    };
    let mut notes: Vec<String> = config_problem.into_iter().collect();
    if config.http.enabled
        && let Err(e) = http::spawn(config.http.port, config.http.token.clone())
    {
        notes.push(format!("HTTP API, port {}: {e}", config.http.port));
    }
    let shared = Arc::new(Shared::new(notes));
    *shared
        .open_query
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = opened.filter(|query| !query.is_empty());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZTHING")
            .with_inner_size([1100.0, 650.0])
            .with_icon(egui::IconData {
                rgba: WINDOW_ICON.to_vec(),
                width: WINDOW_ICON_SIZE,
                height: WINDOW_ICON_SIZE,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "ZTHING",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            ctx.set_theme(match config.theme {
                Theme::System => egui::ThemePreference::System,
                Theme::Light => egui::ThemePreference::Light,
                Theme::Dark => egui::ThemePreference::Dark,
            });
            let (jobs, received_jobs) = mpsc::channel();
            let (replies, received_replies) = mpsc::channel();
            app::spawn_client(shared.clone(), ctx.clone(), received_jobs, replies);
            let (previews, preview_requests) = mpsc::channel();
            let preview_result = Arc::new(std::sync::Mutex::new(None));
            preview::spawn(ctx.clone(), preview_requests, preview_result.clone());
            let (open_shared, open_ctx) = (shared.clone(), ctx.clone());
            std::thread::spawn(move || {
                while open_event.WaitForSingleObject(None).is_ok() {
                    let query = fs::read_to_string(&handoff).unwrap_or_default();
                    if !query.is_empty() {
                        *open_shared
                            .open_query
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(query);
                    }
                    open_shared.show_window();
                    open_ctx.request_repaint();
                }
            });
            let hotkeys = register_hotkey(&config.hotkey, &shared, &ctx)
                .map_err(|e| shared.note(format!("{} {}: {e}", texts.hotkey_failed, config.hotkey)))
                .ok();
            let tray = create_tray(texts, &shared, &ctx)
                .map_err(|e| shared.note(format!("Tray: {e}")))
                .ok();
            Ok(Box::new(App::new(
                shared,
                texts,
                prefix,
                jobs,
                received_replies,
                PreviewPane::new(config.preview, previews, preview_result),
                tray,
                hotkeys,
            )))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

fn register_hotkey(
    hotkey: &str,
    shared: &Arc<Shared>,
    ctx: &egui::Context,
) -> anyhow::Result<GlobalHotKeyManager> {
    let manager = GlobalHotKeyManager::new()?;
    manager.register(hotkey.parse::<HotKey>()?)?;
    let (shared, ctx) = (shared.clone(), ctx.clone());
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state == HotKeyState::Pressed {
            shared.show_window();
            ctx.request_repaint();
        }
    }));
    Ok(manager)
}

fn create_tray(
    texts: &'static Texts,
    shared: &Arc<Shared>,
    ctx: &egui::Context,
) -> anyhow::Result<TrayIcon> {
    let show = MenuItem::new(texts.show, true, None);
    let quit = MenuItem::new(texts.quit, true, None);
    let menu = Menu::new();
    menu.append_items(&[&show, &quit])?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("ZTHING")
        .with_icon(Icon::from_rgba(
            TRAY_ICON.to_vec(),
            TRAY_ICON_SIZE,
            TRAY_ICON_SIZE,
        )?)
        .build()?;

    let quit_id = quit.id().clone();
    let (menu_shared, menu_ctx) = (shared.clone(), ctx.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == quit_id {
            menu_shared.quit.store(true, Ordering::Relaxed);
        }
        // Quitting also needs a visible window, because only then does eframe run the frame that closes it.
        menu_shared.show_window();
        menu_ctx.request_repaint();
    }));
    let (click_shared, click_ctx) = (shared.clone(), ctx.clone());
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            click_shared.show_window();
            click_ctx.request_repaint();
        }
    }));
    Ok(tray)
}
