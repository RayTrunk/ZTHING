//! The window plus the thread that talks to the ZTHING service.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use eframe::egui::{self, Key, Sense, ViewportCommand};
use egui_extras::{Column, TableBuilder};
use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::HotKey;
use tray_icon::TrayIcon;
use winsafe::prelude::*;
use winsafe::{HWND, co};
use zthing_client::{Client, ClientError, Hit, Page, Search, Sort, Status};

use crate::config::Config;
use crate::preview::{Content, Preview};
use crate::settings::{Outcome, Settings};
use crate::shell;
use crate::text::Texts;
use crate::theme;

/// How often the client thread asks for the status, which tells when the index changed.
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
/// Rows per request. The table asks for a page once one of its rows becomes visible.
const PAGE: u64 = 200;
/// `CREATE_NEW_CONSOLE`: a console program started from the window gets a window of its own.
const CREATE_NEW_CONSOLE: u32 = 0x10;

pub enum Service {
    Connecting,
    Online(Status),
    Offline(String),
}

pub struct Shared {
    /// Written by the client thread. Kept here rather than sent through a
    /// channel, so statuses do not pile up while the window is hidden.
    pub service: Mutex<Service>,
    /// Hotkey and tray errors for the status line.
    pub notes: Mutex<Vec<String>>,
    /// Captured on the first frame. eframe runs no frames while the window is
    /// hidden, so hotkey and tray handlers show it through Win32 directly.
    pub window: Mutex<Option<HWND>>,
    pub quit: AtomicBool,
    pub focus_search: AtomicBool,
    /// A query from the URL protocol or a second start, taken over by the next frame.
    pub open_query: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(notes: Vec<String>) -> Self {
        Self {
            service: Mutex::new(Service::Connecting),
            notes: Mutex::new(notes),
            window: Mutex::new(None),
            quit: AtomicBool::new(false),
            focus_search: AtomicBool::new(false),
            open_query: Mutex::new(None),
        }
    }

    pub fn show_window(&self) {
        if let Some(window) = lock(&self.window).as_ref() {
            window.ShowWindow(co::SW::SHOW);
            if window.IsIconic() {
                window.ShowWindow(co::SW::RESTORE);
            }
            window.SetForegroundWindow();
        }
        self.focus_search.store(true, Ordering::Relaxed);
    }

    pub fn note(&self, note: String) {
        lock(&self.notes).push(note);
    }
}

/// A panicking thread must not take the window down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One page of one search. Every new search text, sort or index version is a new generation.
pub struct Job {
    generation: u64,
    search: Search,
}

pub enum Reply {
    Page {
        generation: u64,
        offset: u64,
        page: Page,
    },
    Failed {
        generation: u64,
        error: String,
    },
}

pub fn spawn_client(
    shared: Arc<Shared>,
    ctx: egui::Context,
    jobs: Receiver<Job>,
    replies: Sender<Reply>,
) {
    thread::spawn(move || {
        let mut client: Option<Client> = None;
        let mut last_status: Option<Instant> = None;
        loop {
            let wait = last_status.map_or(Duration::ZERO, |last| {
                STATUS_INTERVAL.saturating_sub(last.elapsed())
            });
            let mut queued: Vec<Job> = match jobs.recv_timeout(wait) {
                Ok(job) => std::iter::once(job).chain(jobs.try_iter()).collect(),
                Err(RecvTimeoutError::Timeout) => Vec::new(),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // Pages of a search the user already replaced are not worth fetching.
            let newest = queued.iter().map(|job| job.generation).max();
            queued.retain(|job| Some(job.generation) == newest);

            let connected = match &mut client {
                Some(connected) => connected,
                None => match Client::connect() {
                    Ok(connected) => client.insert(connected),
                    Err(e) => {
                        *lock(&shared.service) = Service::Offline(e.to_string());
                        last_status = Some(Instant::now());
                        ctx.request_repaint();
                        continue;
                    }
                },
            };
            let mut lost = None;
            for job in queued {
                let offset = job.search.offset;
                let reply = match connected.search(job.search) {
                    Ok(page) => Reply::Page {
                        generation: job.generation,
                        offset,
                        page,
                    },
                    // The service rejected the query, the connection is fine.
                    Err(ClientError::Service(error)) => Reply::Failed {
                        generation: job.generation,
                        error,
                    },
                    Err(e) => {
                        lost = Some(e);
                        break;
                    }
                };
                // The window only stops listening when it closes.
                let _ = replies.send(reply);
                ctx.request_repaint();
            }
            if lost.is_none() && last_status.is_none_or(|last| last.elapsed() >= STATUS_INTERVAL) {
                last_status = Some(Instant::now());
                match connected.status() {
                    Ok(status) => {
                        *lock(&shared.service) = Service::Online(status);
                        ctx.request_repaint();
                    }
                    Err(e) => lost = Some(e),
                }
            }
            if let Some(e) = lost {
                client = None;
                *lock(&shared.service) = Service::Offline(e.to_string());
                ctx.request_repaint();
            }
        }
    });
}

#[derive(Default)]
struct Results {
    generation: u64,
    total: u64,
    millis: f64,
    error: Option<String>,
    pages: HashMap<u64, Vec<Hit>>,
    requested: HashSet<u64>,
}

impl Results {
    fn hit(&self, row: usize) -> Option<&Hit> {
        let row = row as u64;
        self.pages.get(&(row / PAGE))?.get((row % PAGE) as usize)
    }
}

enum Action {
    Open(String),
    OpenWith(String),
    OpenFolder(String),
    Terminal { path: String, folder: bool },
    CopyPath(String),
    CopyName(String),
    Properties(String),
}

/// The pane right of the list. `on` starts from the setting, F3 toggles it.
pub struct PreviewPane {
    on: bool,
    requests: Sender<String>,
    result: Arc<Mutex<Option<Preview>>>,
    shown: Option<Preview>,
    /// The picture of `shown`, uploaded once instead of every frame.
    texture: Option<egui::TextureHandle>,
    /// Path of the last request, to ask for each selection once.
    requested: Option<String>,
}

impl PreviewPane {
    pub fn new(on: bool, requests: Sender<String>, result: Arc<Mutex<Option<Preview>>>) -> Self {
        Self {
            on,
            requests,
            result,
            shown: None,
            texture: None,
            requested: None,
        }
    }

    fn update(&mut self, ctx: &egui::Context, path: Option<&str>) {
        if self.requested.as_deref() != path {
            self.requested = path.map(str::to_owned);
            if let Some(path) = path {
                // The preview thread only stops when the window closes.
                let _ = self.requests.send(path.to_owned());
            }
        }
        if let Some(mut preview) = lock(&self.result).take() {
            self.texture = match std::mem::replace(&mut preview.content, Content::None) {
                Content::Image(image) => {
                    Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR))
                }
                other => {
                    preview.content = other;
                    None
                }
            };
            self.shown = Some(preview);
        }
    }

    fn ui(&self, ui: &mut egui::Ui, texts: &Texts, hit: Option<&Hit>) {
        let Some(hit) = hit else {
            ui.weak(texts.no_selection);
            return;
        };
        let path = hit.path.as_deref().unwrap_or_default();
        let current = self.shown.as_ref().filter(|preview| preview.path == path);
        ui.add(
            egui::Label::new(egui::RichText::new(hit.name.as_deref().unwrap_or_default()).strong())
                .wrap(),
        );
        ui.add(egui::Label::new(egui::RichText::new(path).weak()).wrap());
        egui::Grid::new("preview-facts")
            .num_columns(2)
            .show(ui, |ui| {
                if hit.folder != Some(true) {
                    ui.label(texts.size);
                    ui.label(format_size(hit.size.unwrap_or(0)));
                    ui.end_row();
                }
                ui.label(texts.modified);
                ui.label(format_date(hit.modified.unwrap_or(0)));
                ui.end_row();
                if let Some(created) = current.and_then(|preview| preview.created) {
                    ui.label(texts.created);
                    ui.label(format_date(created));
                    ui.end_row();
                }
                if let Some(letters) = current
                    .map(|preview| crate::preview::attribute_letters(preview.attributes))
                    .filter(|letters| !letters.is_empty())
                {
                    ui.label(texts.attributes);
                    ui.label(letters);
                    ui.end_row();
                }
            });
        ui.separator();
        match (current, &self.texture) {
            (None, _) => {
                ui.spinner();
            }
            (
                Some(Preview {
                    content: Content::Text(text),
                    ..
                }),
                _,
            ) => {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).extend());
                });
            }
            (Some(_), Some(texture)) => {
                ui.add(egui::Image::new(texture).max_size(ui.available_size()));
            }
            (Some(_), None) => {
                ui.weak(texts.no_preview);
            }
        }
    }
}

pub struct App {
    shared: Arc<Shared>,
    texts: &'static Texts,
    config: Config,
    config_path: PathBuf,
    /// Open while the settings window shows.
    settings: Option<Settings>,
    show_info: bool,
    logo: Option<egui::TextureHandle>,
    input: String,
    sort: Sort,
    descending: bool,
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
    /// Stays on screen until the first page of the next search arrives, so typing does not flicker.
    shown: Results,
    generation: u64,
    selected: Option<usize>,
    scroll_to_selected: bool,
    /// Input, sort and index version of the last search, to send each search once.
    sent: Option<(String, Sort, bool, Option<u64>)>,
    preview: PreviewPane,
    _tray: Option<TrayIcon>,
    hotkeys: Option<GlobalHotKeyManager>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Arc<Shared>,
        texts: &'static Texts,
        config: Config,
        config_path: PathBuf,
        jobs: Sender<Job>,
        replies: Receiver<Reply>,
        preview: PreviewPane,
        tray: Option<TrayIcon>,
        hotkeys: Option<GlobalHotKeyManager>,
    ) -> Self {
        Self {
            shared,
            texts,
            config,
            config_path,
            settings: None,
            show_info: false,
            logo: None,
            input: String::new(),
            sort: Sort::Name,
            descending: false,
            jobs,
            replies,
            shown: Results::default(),
            generation: 0,
            selected: None,
            scroll_to_selected: false,
            sent: None,
            preview,
            _tray: tray,
            hotkeys,
        }
    }

    fn request(&self, generation: u64, page: u64) {
        // The client thread only stops when the window closes, so a failed send can be ignored.
        let _ = self.jobs.send(Job {
            generation,
            search: Search {
                query: format!("{}{}", self.config.query_prefix(), self.input),
                sort: self.sort,
                descending: self.descending,
                offset: page * PAGE,
                limit: PAGE,
                ..Search::default()
            },
        });
    }

    /// `index_version` is `None` while the service is unreachable or still
    /// reading the drives. Forgetting the sent search then repeats it once the
    /// service answers, which also fetches pages lost with the connection.
    fn search(&mut self, index_version: Option<u64>) {
        if index_version.is_none() {
            self.sent = None;
            return;
        }
        let key = (
            self.input.clone(),
            self.sort,
            self.descending,
            index_version,
        );
        if self.sent.as_ref() == Some(&key) {
            return;
        }
        self.generation += 1;
        self.request(self.generation, 0);
        self.sent = Some(key);
    }

    fn receive(&mut self) {
        while let Ok(reply) = self.replies.try_recv() {
            match reply {
                Reply::Page {
                    generation,
                    offset,
                    page,
                } => {
                    if generation != self.shown.generation {
                        if generation != self.generation {
                            continue;
                        }
                        // Later pages come from the service's cached result, so
                        // only the first one tells how long the search took.
                        self.shown = Results {
                            generation,
                            millis: page.millis,
                            ..Results::default()
                        };
                        self.selected = self.selected.filter(|&row| (row as u64) < page.total);
                    }
                    self.shown.total = page.total;
                    self.shown.pages.insert(offset / PAGE, page.hits);
                }
                Reply::Failed { generation, error } if generation == self.generation => {
                    self.shown = Results {
                        generation,
                        error: Some(error),
                        ..Results::default()
                    };
                    self.selected = None;
                }
                Reply::Failed { .. } => {}
            }
        }
    }

    fn selected_path(&self) -> Option<String> {
        self.shown.hit(self.selected.unwrap_or(0))?.path.clone()
    }

    fn keyboard(&mut self, ctx: &egui::Context) -> Option<Action> {
        let (down, up, enter, ctrl, copy) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::Enter),
                i.modifiers.ctrl,
                // egui turns Ctrl+C into a copy event, also with Shift held.
                i.modifiers.ctrl
                    && i.modifiers.shift
                    && (i.key_pressed(Key::C) || i.events.contains(&egui::Event::Copy)),
            )
        });
        let last = (self.shown.total as usize).checked_sub(1)?;
        if down || up {
            self.selected = Some(match (self.selected, down) {
                (None, _) => 0,
                (Some(row), true) => (row + 1).min(last),
                (Some(row), false) => row.saturating_sub(1),
            });
            self.scroll_to_selected = true;
        }
        let path = self.selected_path()?;
        match (enter, ctrl, copy) {
            (_, _, true) => Some(Action::CopyPath(path)),
            (true, true, _) => Some(Action::OpenFolder(path)),
            (true, false, _) => Some(Action::Open(path)),
            _ => None,
        }
    }

    /// Applies a saved settings window and writes config.toml. HTTP changes
    /// wait for the next start, because the server thread cannot be stopped.
    fn apply(&mut self, ctx: &egui::Context, config: Config) {
        theme::apply(ctx, config.theme);
        if config.preview != self.config.preview {
            self.preview.on = config.preview;
        }
        if config.hotkey != self.config.hotkey
            && let Some(manager) = &self.hotkeys
        {
            if let Ok(old) = self.config.hotkey.parse::<HotKey>() {
                // It may never have been registered, for example when another program holds it.
                let _ = manager.unregister(old);
            }
            let registered = config
                .hotkey
                .parse::<HotKey>()
                .map_err(|e| e.to_string())
                .and_then(|hotkey| manager.register(hotkey).map_err(|e| e.to_string()));
            let mut notes = lock(&self.shared.notes);
            notes.retain(|note| !note.starts_with(self.texts.hotkey_failed));
            if let Err(e) = registered {
                notes.push(format!(
                    "{} {}: {e}",
                    self.texts.hotkey_failed, config.hotkey
                ));
            }
        }
        if config.drives != self.config.drives || config.exclude != self.config.exclude {
            // Searches again with the new query prefix.
            self.sent = None;
        }
        if let Err(e) = config.save(&self.config_path) {
            self.shared
                .note(format!("{}: {e}", self.config_path.display()));
        }
        self.config = config;
    }

    fn info_ui(&mut self, ctx: &egui::Context) {
        let texts = self.texts;
        let logo = self
            .logo
            .get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [crate::WINDOW_ICON_SIZE as usize; 2],
                    crate::WINDOW_ICON,
                );
                ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
            })
            .clone();
        let service = match &*lock(&self.shared.service) {
            Service::Online(status) => {
                let drives: Vec<String> = status
                    .volumes
                    .iter()
                    .map(|volume| {
                        let live = if volume.live { "" } else { " *" };
                        format!("{}:{live}", volume.drive)
                    })
                    .collect();
                format!(
                    "{} {}, {} {}",
                    status.entries,
                    texts.entries,
                    texts.drives,
                    drives.join(" ")
                )
            }
            Service::Offline(error) => format!("{}: {error}", texts.no_service),
            Service::Connecting => texts.indexing.to_owned(),
        };
        let mut open = true;
        let mut open_folder = false;
        egui::Window::new(texts.info)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(&logo).fit_to_exact_size(egui::vec2(64.0, 64.0)));
                    ui.vertical(|ui| {
                        ui.heading("ZTHING");
                        ui.label(format!("{} {}", texts.version, env!("CARGO_PKG_VERSION")));
                        ui.label(texts.about);
                    });
                });
                ui.separator();
                egui::Grid::new("info")
                    .num_columns(2)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(texts.service);
                        ui.label(service);
                        ui.end_row();
                        ui.label(texts.license);
                        ui.label("MIT OR Apache-2.0");
                        ui.end_row();
                        ui.label(texts.settings);
                        ui.horizontal(|ui| {
                            ui.label(self.config_path.display().to_string());
                            open_folder = ui.small_button(texts.open_folder).clicked();
                        });
                        ui.end_row();
                    });
                ui.separator();
                ui.strong(texts.shortcuts);
                egui::Grid::new("shortcuts")
                    .num_columns(2)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for (keys, what) in [
                            (self.config.hotkey.as_str(), texts.show),
                            ("↑ ↓", texts.selection),
                            ("Enter", texts.open),
                            ("Ctrl+Enter", texts.open_folder),
                            ("Ctrl+Shift+C", texts.copy_path),
                            ("F3", texts.preview),
                        ] {
                            ui.monospace(keys);
                            ui.label(what);
                            ui.end_row();
                        }
                    });
            });
        if open_folder {
            self.run(
                ctx,
                Action::OpenFolder(self.config_path.display().to_string()),
            );
        }
        self.show_info &= open;
    }

    fn run(&self, ctx: &egui::Context, action: Action) {
        let (program, started) = match action {
            Action::CopyPath(text) | Action::CopyName(text) => {
                ctx.copy_text(text);
                return;
            }
            Action::Properties(path) => {
                if let Err(e) = shell::properties(&path) {
                    self.shared.note(format!("{}: {e}", self.texts.properties));
                }
                return;
            }
            Action::Open(path) => ("explorer", Command::new("explorer").arg(&path).spawn()),
            // Explorer only parses the quoted form of `/select,`.
            Action::OpenFolder(path) => (
                "explorer",
                Command::new("explorer")
                    .raw_arg(format!("/select,\"{path}\""))
                    .spawn(),
            ),
            // OpenAs_RunDLL takes the rest of the command line as the path, unquoted.
            Action::OpenWith(path) => (
                "rundll32",
                Command::new("rundll32")
                    .raw_arg(format!("shell32.dll,OpenAs_RunDLL {path}"))
                    .spawn(),
            ),
            Action::Terminal { path, folder } => {
                let dir = if folder {
                    Path::new(&path)
                } else {
                    Path::new(&path).parent().unwrap_or(Path::new(&path))
                };
                // Windows Terminal where installed, PowerShell otherwise.
                let started = Command::new("wt").arg("-d").arg(dir).spawn().or_else(|_| {
                    Command::new("powershell")
                        .arg("-NoExit")
                        .current_dir(dir)
                        .creation_flags(CREATE_NEW_CONSOLE)
                        .spawn()
                });
                ("terminal", started)
            }
        };
        if let Err(e) = started {
            self.shared.note(format!("{program}: {e}"));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        {
            let mut window = lock(&self.shared.window);
            if window.is_none() {
                *window = HWND::GetActiveWindow();
            }
        }
        if self.shared.quit.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            // Closing hides the window. "Quit" in the tray menu ends ZTHING.
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if let Some(window) = lock(&self.shared.window).as_ref() {
                window.ShowWindow(co::SW::HIDE);
            }
        }

        if let Some(query) = lock(&self.shared.open_query).take() {
            self.input = query;
        }
        let (ready, index_version, problem, service_notes) = match &*lock(&self.shared.service) {
            Service::Connecting => (false, None, None, Vec::new()),
            Service::Online(status) => (
                status.ready,
                status.ready.then_some(status.index_version),
                None,
                status.notes.clone(),
            ),
            Service::Offline(error) => (false, None, Some(error.clone()), Vec::new()),
        };
        self.search(index_version);
        self.receive();
        // Keys belong to the settings window while it is open.
        let mut action = None;
        if self.settings.is_none() {
            action = self.keyboard(ctx);
            if ctx.input(|i| i.key_pressed(Key::F3)) {
                self.preview.on = !self.preview.on;
            }
        }
        let texts = self.texts;
        let selected_hit = self.selected.and_then(|row| self.shown.hit(row)).cloned();
        if self.preview.on {
            self.preview.update(
                ctx,
                selected_hit.as_ref().and_then(|hit| hit.path.as_deref()),
            );
        }

        let mut open_settings = false;
        egui::TopBottomPanel::top("search").show(ctx, |ui| {
            ui.add_space(4.0);
            // A right-to-left layout fills the panel's height; a fixed row height keeps it one line.
            let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
            ui.allocate_ui_with_layout(
                row,
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui.button("ℹ").on_hover_text(texts.info).clicked() {
                        self.show_info = !self.show_info;
                    }
                    open_settings = ui.button("⚙").on_hover_text(texts.settings).clicked();
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.input)
                            .hint_text(texts.search_hint)
                            .desired_width(ui.available_width()),
                    );
                    let wanted = self.shared.focus_search.swap(false, Ordering::Relaxed);
                    if wanted || (self.settings.is_none() && !response.has_focus()) {
                        response.request_focus();
                    }
                },
            );
            ui.add_space(4.0);
        });
        if open_settings && self.settings.is_none() {
            self.settings = Some(Settings::new(&self.config));
        }
        if let Some(settings) = &mut self.settings {
            match settings.ui(ctx, texts) {
                Outcome::Open => {}
                Outcome::Save(config) => {
                    self.settings = None;
                    self.apply(ctx, config);
                }
                Outcome::Cancel => {
                    self.settings = None;
                    theme::apply(ctx, self.config.theme);
                }
            }
        }
        if self.show_info {
            self.info_ui(ctx);
        }

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(problem) = &problem {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("{}: {problem}", texts.no_service),
                    );
                } else if ready {
                    ui.label(format!(
                        "{} {} · {:.1} ms",
                        self.shown.total, texts.objects, self.shown.millis
                    ));
                } else {
                    ui.label(texts.indexing);
                }
                if let Some(error) = &self.shown.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                for note in lock(&self.shared.notes).iter().chain(&service_notes) {
                    ui.colored_label(ui.visuals().warn_fg_color, note);
                }
            });
        });

        if self.preview.on {
            egui::SidePanel::right("preview")
                .resizable(true)
                .default_width(360.0)
                .show(ctx, |ui| self.preview.ui(ui, texts, selected_hit.as_ref()));
        }

        let mut sort_by = None;
        let mut missing = Vec::new();
        egui::CentralPanel::default().show(ctx, |ui| {
            let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            // Selectable labels take the clicks over their text, which left only
            // the strip below the text to select a row with.
            ui.style_mut().interaction.selectable_labels = false;
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .column(Column::initial(260.0).at_least(80.0).clip(true))
                .column(Column::initial(440.0).at_least(80.0).clip(true))
                .column(Column::initial(90.0).at_least(50.0))
                .column(Column::remainder().at_least(120.0));
            if std::mem::take(&mut self.scroll_to_selected)
                && let Some(row) = self.selected
            {
                table = table.scroll_to_row(row, None);
            }
            let (sort, descending) = (self.sort, self.descending);
            let shown = &self.shown;
            let selected = &mut self.selected;
            table
                .header(row_height, |mut header| {
                    for (key, label) in [
                        (Sort::Name, texts.name),
                        (Sort::Path, texts.folder),
                        (Sort::Size, texts.size),
                        (Sort::Modified, texts.modified),
                    ] {
                        header.col(|ui| {
                            let arrow = match (sort == key, descending) {
                                (false, _) => "",
                                // The bundled icon font has these arrows, the text font has no triangles.
                                (true, false) => " ⏶",
                                (true, true) => " ⏷",
                            };
                            let label = egui::Label::new(
                                egui::RichText::new(format!("{label}{arrow}")).strong(),
                            )
                            .sense(Sense::click());
                            if ui.add(label).clicked() {
                                sort_by = Some(key);
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, shown.total as usize, |mut row| {
                        let position = row.index();
                        let Some(hit) = shown.hit(position) else {
                            let page = position as u64 / PAGE;
                            if !shown.requested.contains(&page) && !missing.contains(&page) {
                                missing.push(page);
                            }
                            return;
                        };
                        row.set_selected(*selected == Some(position));
                        let path = hit.path.clone().unwrap_or_default();
                        let name = hit.name.clone().unwrap_or_default();
                        let is_folder = hit.folder == Some(true);
                        let folder = path.rsplit_once('\\').map_or("", |(folder, _)| folder);
                        row.col(|ui| {
                            ui.label(&name);
                        });
                        row.col(|ui| {
                            ui.label(folder);
                        });
                        row.col(|ui| {
                            if !is_folder {
                                ui.label(format_size(hit.size.unwrap_or(0)));
                            }
                        });
                        row.col(|ui| {
                            ui.label(format_date(hit.modified.unwrap_or(0)));
                        });
                        let response = row.response();
                        if response.clicked() || response.secondary_clicked() {
                            *selected = Some(position);
                        }
                        if response.double_clicked() {
                            action = Some(Action::Open(path.clone()));
                        }
                        response.context_menu(|ui| {
                            let entries = [
                                (texts.open, Action::Open(path.clone())),
                                (texts.open_with, Action::OpenWith(path.clone())),
                                (texts.open_folder, Action::OpenFolder(path.clone())),
                                (
                                    texts.terminal,
                                    Action::Terminal {
                                        path: path.clone(),
                                        folder: is_folder,
                                    },
                                ),
                                (texts.copy_path, Action::CopyPath(path.clone())),
                                (texts.copy_name, Action::CopyName(name.clone())),
                                (texts.properties, Action::Properties(path.clone())),
                            ];
                            for (position, (label, entry)) in entries.into_iter().enumerate() {
                                // Groups: open, copy, properties.
                                if position == 4 || position == 6 {
                                    ui.separator();
                                }
                                if ui.button(label).clicked() {
                                    action = Some(entry);
                                    ui.close();
                                }
                            }
                        });
                    });
                });
        });

        // Only pages of the shown search: a newer search fetches its own first page.
        if self.shown.generation == self.generation {
            for page in missing {
                self.request(self.generation, page);
                self.shown.requested.insert(page);
            }
        }
        if let Some(key) = sort_by {
            self.descending = self.sort == key && !self.descending;
            self.sort = key;
        }
        if let Some(action) = action {
            self.run(ctx, action);
        }
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_date(unix: i64) -> String {
    Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}
