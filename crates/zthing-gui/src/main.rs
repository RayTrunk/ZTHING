// No console window next to the GUI.
#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use eframe::egui;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use zthing_gui::app::{self, App, Shared};
use zthing_gui::config::{self, Theme};
use zthing_gui::text::{self, Texts};

const ICON_SIZE: u32 = 32;

fn main() -> anyhow::Result<()> {
    let texts = text::system();
    let app_data = std::env::var_os("APPDATA").context("APPDATA is not set")?;
    let local_app_data = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let (config, config_problem) =
        config::load(&PathBuf::from(app_data).join(r"ZTHING\config.toml"));
    let index_path = PathBuf::from(local_app_data).join(r"ZTHING\index.bin");
    let drives: Vec<char> = if config.drives.is_empty() {
        zthing_ntfs::fixed_drives()
    } else {
        config
            .drives
            .iter()
            .filter_map(|drive| drive.chars().next())
            .map(|drive| drive.to_ascii_uppercase())
            .collect()
    };
    // The trailing backslash hides the folder's contents but not folders that
    // merely start with the same name.
    let exclusions: String = config
        .exclude
        .iter()
        .map(|folder| format!("!path:\"{}\\\" ", folder.trim_end_matches('\\')))
        .collect();
    let shared = Arc::new(Shared::new(config_problem.into_iter().collect()));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZTHING")
            .with_inner_size([1100.0, 650.0])
            .with_icon(egui::IconData {
                rgba: icon_rgba(),
                width: ICON_SIZE,
                height: ICON_SIZE,
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
            app::spawn_indexer(
                shared.clone(),
                ctx.clone(),
                index_path.clone(),
                drives,
                texts,
            );
            let (requests, received) = mpsc::channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let results = Arc::new(Mutex::new(None));
            app::spawn_searcher(
                shared.clone(),
                ctx.clone(),
                received,
                cancel.clone(),
                results.clone(),
                exclusions,
            );
            let hotkeys = register_hotkey(&config.hotkey, &shared, &ctx)
                .map_err(|e| shared.note(format!("{} {}: {e}", texts.hotkey_failed, config.hotkey)))
                .ok();
            let tray = create_tray(texts, &shared, &ctx)
                .map_err(|e| shared.note(format!("Tray: {e}")))
                .ok();
            Ok(Box::new(App::new(
                shared, texts, index_path, requests, cancel, results, tray, hotkeys,
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
        .with_icon(Icon::from_rgba(icon_rgba(), ICON_SIZE, ICON_SIZE)?)
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

/// A magnifying glass, drawn in code so the binary needs no image files.
fn icon_rgba() -> Vec<u8> {
    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let distance = ((fx - 13.0).powi(2) + (fy - 13.0).powi(2)).sqrt();
            let lens = (8.0..11.5).contains(&distance);
            let handle = fx > 19.0 && fx < 30.0 && (fx - fy).abs() < 3.0;
            if lens || handle {
                let at = ((y * ICON_SIZE + x) * 4) as usize;
                rgba[at..at + 4].copy_from_slice(&[0x1E, 0x6F, 0xD9, 0xFF]);
            }
        }
    }
    rgba
}
