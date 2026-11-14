//! The window plus the threads that keep the index current and run searches.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use eframe::egui::{self, Key, Sense, ViewportCommand};
use egui_extras::{Column, TableBuilder};
use global_hotkey::GlobalHotKeyManager;
use tray_icon::TrayIcon;
use winsafe::prelude::*;
use winsafe::{HWND, co};
use zthing_core::{Now, Query, SortKey};
use zthing_ntfs::{Indexer, NtfsError};

use crate::text::Texts;

/// Journal polling interval. Changes appear within this time plus a few milliseconds.
const UPDATE_INTERVAL: Duration = Duration::from_millis(250);
/// How often the indexer thread asks [`Indexer::needs_save`].
const SAVE_CHECK_INTERVAL: Duration = Duration::from_secs(300);

pub struct Shared {
    pub indexer: RwLock<Option<Indexer>>,
    /// Raised after every change to the index, so the shown search runs again.
    pub version: AtomicU64,
    /// Skipped drives and errors for the status line.
    pub notes: Mutex<Vec<String>>,
    /// Captured on the first frame. eframe runs no frames while the window is
    /// hidden, so hotkey and tray handlers show it through Win32 directly.
    pub window: Mutex<Option<HWND>>,
    pub quit: AtomicBool,
    pub focus_search: AtomicBool,
}

impl Shared {
    pub fn new(notes: Vec<String>) -> Self {
        Self {
            indexer: RwLock::new(None),
            version: AtomicU64::new(0),
            notes: Mutex::new(notes),
            window: Mutex::new(None),
            quit: AtomicBool::new(false),
            focus_search: AtomicBool::new(false),
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
        let mut notes = lock(&self.notes);
        // A failing update repeats every interval, the status line shows it once.
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
}

/// A panicking thread must not take the window down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn spawn_indexer(
    shared: Arc<Shared>,
    ctx: egui::Context,
    path: PathBuf,
    drives: Vec<char>,
    texts: &'static Texts,
) {
    thread::spawn(move || {
        let (indexer, report) = match Indexer::open(&path, &drives) {
            Ok(opened) => opened,
            Err(e) => {
                shared.note(e.to_string());
                ctx.request_repaint();
                return;
            }
        };
        for e in &report.skipped {
            shared.note(format!("{}: {e}", texts.skipped));
        }
        *shared
            .indexer
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(indexer);
        shared.version.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();

        let mut last_save_check = Instant::now();
        while !shared.quit.load(Ordering::Relaxed) {
            thread::sleep(UPDATE_INTERVAL);
            let mut guard = shared
                .indexer
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(indexer) = guard.as_mut() else {
                return;
            };
            let result = match indexer.update() {
                Err(NtfsError::Journal { .. }) => indexer.rebuild().map(|failed| {
                    for e in failed {
                        shared.note(format!("{}: {e}", texts.skipped));
                    }
                    usize::MAX
                }),
                other => other,
            };
            match result {
                Ok(0) => {}
                Ok(_) => {
                    shared.version.fetch_add(1, Ordering::Relaxed);
                    ctx.request_repaint();
                }
                Err(e) => shared.note(e.to_string()),
            }
            if last_save_check.elapsed() >= SAVE_CHECK_INTERVAL {
                last_save_check = Instant::now();
                if indexer.needs_save()
                    && let Err(e) = indexer.save(&path)
                {
                    shared.note(format!("{}: {e}", path.display()));
                }
            }
        }
    });
}

pub struct Request {
    input: String,
    sort: SortKey,
    descending: bool,
    version: u64,
}

#[derive(Default)]
pub struct Outcome {
    ids: Vec<u32>,
    millis: f64,
    error: Option<String>,
}

/// `exclusions` is prepended to every query, which keeps [`Query::narrows`] working.
pub fn spawn_searcher(
    shared: Arc<Shared>,
    ctx: egui::Context,
    requests: Receiver<Request>,
    cancel: Arc<AtomicBool>,
    results: Arc<Mutex<Option<Outcome>>>,
    exclusions: String,
) {
    thread::spawn(move || {
        let mut previous: Option<(Query, Vec<u32>, u64)> = None;
        while let Ok(mut request) = requests.recv() {
            // Only the newest input matters.
            while let Ok(newer) = requests.try_recv() {
                request = newer;
            }
            cancel.store(false, Ordering::Relaxed);
            let start = Instant::now();
            let query = match Query::parse(&format!("{exclusions}{}", request.input), now()) {
                Ok(query) => query,
                Err(e) => {
                    *lock(&results) = Some(Outcome {
                        error: Some(e.to_string()),
                        ..Outcome::default()
                    });
                    ctx.request_repaint();
                    continue;
                }
            };
            let guard = shared
                .indexer
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(indexer) = guard.as_ref() else {
                continue;
            };
            let index = indexer.index();
            let within = previous
                .as_ref()
                .filter(|(old, _, version)| *version == request.version && query.narrows(old))
                .map(|(_, ids, _)| ids.as_slice());
            let Some(mut ids) = index.search(&query, within, &cancel) else {
                continue;
            };
            index.sort(&mut ids, request.sort);
            if request.descending {
                ids.reverse();
            }
            let millis = start.elapsed().as_secs_f64() * 1e3;
            drop(guard);
            previous = Some((query, ids.clone(), request.version));
            *lock(&results) = Some(Outcome {
                ids,
                millis,
                error: None,
            });
            ctx.request_repaint();
        }
    });
}

fn now() -> Now {
    Now {
        unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        utc_offset: i64::from(Local::now().offset().local_minus_utc()),
    }
}

enum Action {
    Open(String),
    OpenFolder(String),
    CopyPath(String),
}

pub struct App {
    shared: Arc<Shared>,
    texts: &'static Texts,
    index_path: PathBuf,
    input: String,
    sort: SortKey,
    descending: bool,
    requests: Sender<Request>,
    cancel: Arc<AtomicBool>,
    results: Arc<Mutex<Option<Outcome>>>,
    shown: Outcome,
    selected: Option<usize>,
    scroll_to_selected: bool,
    /// Input, sort and index version of the last request, to send each search once.
    sent: Option<(String, SortKey, bool, u64)>,
    _tray: Option<TrayIcon>,
    _hotkeys: Option<GlobalHotKeyManager>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Arc<Shared>,
        texts: &'static Texts,
        index_path: PathBuf,
        requests: Sender<Request>,
        cancel: Arc<AtomicBool>,
        results: Arc<Mutex<Option<Outcome>>>,
        tray: Option<TrayIcon>,
        hotkeys: Option<GlobalHotKeyManager>,
    ) -> Self {
        Self {
            shared,
            texts,
            index_path,
            input: String::new(),
            sort: SortKey::Name,
            descending: false,
            requests,
            cancel,
            results,
            shown: Outcome::default(),
            selected: None,
            scroll_to_selected: false,
            sent: None,
            _tray: tray,
            _hotkeys: hotkeys,
        }
    }

    fn send_request(&mut self) {
        let version = self.shared.version.load(Ordering::Relaxed);
        let key = (self.input.clone(), self.sort, self.descending, version);
        if self.sent.as_ref() == Some(&key) {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        // The searcher only stops when the window closes, so a failed send can be ignored.
        let _ = self.requests.send(Request {
            input: self.input.clone(),
            sort: self.sort,
            descending: self.descending,
            version,
        });
        self.sent = Some(key);
    }

    fn selected_path(&self) -> Option<String> {
        let id = *self.shown.ids.get(self.selected.unwrap_or(0))?;
        let guard = self.shared.indexer.try_read().ok()?;
        Some(guard.as_ref()?.index().path(id))
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
        let last = self.shown.ids.len().checked_sub(1)?;
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

    fn run(&self, ctx: &egui::Context, action: Action) {
        let started = match action {
            Action::CopyPath(path) => {
                ctx.copy_text(path);
                return;
            }
            Action::Open(path) => Command::new("explorer").arg(&path).spawn(),
            // Explorer only parses the quoted form of `/select,`.
            Action::OpenFolder(path) => Command::new("explorer")
                .raw_arg(format!("/select,\"{path}\""))
                .spawn(),
        };
        if let Err(e) = started {
            self.shared.note(format!("explorer: {e}"));
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
            if let Some(indexer) = self
                .shared
                .indexer
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                // The window is closing, so there is nowhere left to show a save error.
                // The next start replays the journal or reads the drives again.
                let _ = indexer.save(&self.index_path);
            }
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

        self.send_request();
        if let Some(outcome) = lock(&self.results).take() {
            self.selected = self.selected.filter(|&row| row < outcome.ids.len());
            self.shown = outcome;
        }
        let mut action = self.keyboard(ctx);
        let texts = self.texts;

        egui::TopBottomPanel::top("search").show(ctx, |ui| {
            ui.add_space(4.0);
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .hint_text(texts.search_hint)
                    .desired_width(f32::INFINITY),
            );
            if self.shared.focus_search.swap(false, Ordering::Relaxed) || !response.has_focus() {
                response.request_focus();
            }
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let ready = self
                    .shared
                    .indexer
                    .try_read()
                    .map_or(true, |guard| guard.is_some());
                if ready {
                    ui.label(format!(
                        "{} {} · {:.1} ms",
                        self.shown.ids.len(),
                        texts.objects,
                        self.shown.millis
                    ));
                } else {
                    ui.label(texts.indexing);
                }
                if let Some(error) = &self.shown.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                for note in lock(&self.shared.notes).iter() {
                    ui.colored_label(ui.visuals().warn_fg_color, note);
                }
            });
        });

        let mut sort_by = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            let Ok(guard) = self.shared.indexer.try_read() else {
                ui.label(texts.updating);
                return;
            };
            let Some(indexer) = guard.as_ref() else {
                ui.label(texts.indexing);
                return;
            };
            let index = indexer.index();
            let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
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
            let ids = &self.shown.ids;
            let selected = &mut self.selected;
            table
                .header(row_height, |mut header| {
                    for (key, label) in [
                        (SortKey::Name, texts.name),
                        (SortKey::Path, texts.folder),
                        (SortKey::Size, texts.size),
                        (SortKey::Modified, texts.modified),
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
                    body.rows(row_height, ids.len(), |mut row| {
                        let position = row.index();
                        let id = ids[position];
                        row.set_selected(*selected == Some(position));
                        let path = index.path(id);
                        let folder = path.rsplit_once('\\').map_or("", |(folder, _)| folder);
                        row.col(|ui| {
                            ui.label(index.name(id));
                        });
                        row.col(|ui| {
                            ui.label(folder);
                        });
                        row.col(|ui| {
                            if !index.is_dir(id) {
                                ui.label(format_size(index.size(id)));
                            }
                        });
                        row.col(|ui| {
                            ui.label(format_date(index.modified(id)));
                        });
                        let response = row.response();
                        if response.clicked() || response.secondary_clicked() {
                            *selected = Some(position);
                        }
                        if response.double_clicked() {
                            action = Some(Action::Open(path.clone()));
                        }
                        response.context_menu(|ui| {
                            if ui.button(texts.open).clicked() {
                                action = Some(Action::Open(path.clone()));
                                ui.close();
                            }
                            if ui.button(texts.open_folder).clicked() {
                                action = Some(Action::OpenFolder(path.clone()));
                                ui.close();
                            }
                            if ui.button(texts.copy_path).clicked() {
                                action = Some(Action::CopyPath(path.clone()));
                                ui.close();
                            }
                        });
                    });
                });
        });

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
