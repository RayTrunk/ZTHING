//! The settings window. It edits a copy of the config: Save hands the copy
//! to the window, which applies and writes it; Cancel drops it.

use std::hash::{BuildHasher, RandomState};
use std::time::SystemTime;

use eframe::egui;
use global_hotkey::hotkey::HotKey;

use crate::config::Config;
use crate::text::Texts;
use crate::theme::{self, Theme};

pub enum Outcome {
    Open,
    Save(Config),
    Cancel,
}

pub struct Settings {
    draft: Config,
    /// Edited as text and parsed on Save, so typing never fights a parser.
    drives: String,
    exclude: String,
    port: String,
    error: Option<&'static str>,
}

impl Settings {
    pub fn new(config: &Config) -> Self {
        Self {
            draft: config.clone(),
            drives: config.drives.join(", "),
            exclude: config.exclude.join("\n"),
            port: config.http.port.to_string(),
            error: None,
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, texts: &Texts) -> Outcome {
        let mut open = true;
        let mut outcome = Outcome::Open;
        egui::Window::new(texts.settings)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("settings")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| self.fields(ui, ctx, texts));
                ui.add_space(6.0);
                ui.weak(texts.http_restart);
                if let Some(error) = self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(texts.save).clicked() {
                        match self.parse(texts) {
                            Ok(config) => outcome = Outcome::Save(config),
                            Err(error) => self.error = Some(error),
                        }
                    }
                    if ui.button(texts.cancel).clicked() {
                        outcome = Outcome::Cancel;
                    }
                });
            });
        if open { outcome } else { Outcome::Cancel }
    }

    fn fields(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, texts: &Texts) {
        ui.label(texts.theme);
        let before = self.draft.theme;
        egui::ComboBox::from_id_salt("theme")
            .selected_text(self.draft.theme.label(texts.system_theme))
            .show_ui(ui, |ui| {
                for theme in Theme::ALL {
                    ui.selectable_value(
                        &mut self.draft.theme,
                        theme,
                        theme.label(texts.system_theme),
                    );
                }
            });
        // Shown at once; Cancel puts the saved theme back.
        if self.draft.theme != before {
            theme::apply(ctx, self.draft.theme);
        }
        ui.end_row();

        ui.label(texts.preview);
        ui.checkbox(&mut self.draft.preview, texts.preview_start);
        ui.end_row();

        ui.label(texts.hotkey);
        ui.add(egui::TextEdit::singleline(&mut self.draft.hotkey).hint_text("Ctrl+Shift+F"));
        ui.end_row();

        ui.label(texts.drives);
        ui.add(egui::TextEdit::singleline(&mut self.drives).hint_text(texts.drives_hint));
        ui.end_row();

        ui.label(texts.exclude);
        ui.add(
            egui::TextEdit::multiline(&mut self.exclude)
                .hint_text(texts.exclude_hint)
                .desired_rows(3),
        );
        ui.end_row();

        ui.label(texts.http);
        ui.checkbox(&mut self.draft.http.enabled, texts.http_enabled);
        ui.end_row();

        ui.label(texts.port);
        ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(80.0));
        ui.end_row();

        ui.label(texts.token);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.draft.http.token).desired_width(260.0));
            if ui.button(texts.new_token).clicked() {
                self.draft.http.token = new_token();
            }
        });
        ui.end_row();
    }

    fn parse(&self, texts: &Texts) -> Result<Config, &'static str> {
        let mut config = self.draft.clone();
        config.drives = self
            .drives
            .split([',', ';', ' '])
            .map(|drive| drive.trim().trim_end_matches([':', '\\']))
            .filter(|drive| !drive.is_empty())
            .map(str::to_uppercase)
            .collect();
        if config
            .drives
            .iter()
            .any(|drive| drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()))
        {
            return Err(texts.bad_drives);
        }
        config.exclude = self
            .exclude
            .lines()
            .map(str::trim)
            .filter(|folder| !folder.is_empty())
            .map(str::to_owned)
            .collect();
        config.hotkey = config.hotkey.trim().to_owned();
        if config.hotkey.parse::<HotKey>().is_err() {
            return Err(texts.bad_hotkey);
        }
        config.http.port = match self.port.trim().parse() {
            Ok(port) if port > 0 => port,
            _ => return Err(texts.bad_port),
        };
        config.http.token = config.http.token.trim().to_owned();
        if config.http.enabled && config.http.token.is_empty() {
            return Err(texts.token_needed);
        }
        Ok(config)
    }
}

/// 64 hex digits. `RandomState` seeds its keys from the operating system's
/// random source, which spares a dependency for one token.
fn new_token() -> String {
    let now = SystemTime::now();
    (0..4u8)
        .map(|part| format!("{:016x}", RandomState::new().hash_one((part, now))))
        .collect()
}
