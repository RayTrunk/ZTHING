//! Colour themes, shared with the Quill products (E:\Claude\hvk): the two
//! Quill themes come from the Quill logo, the others from the Quill editor.

use eframe::egui::{self, Color32, Stroke};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// Quill Dark or Quill Light, following the Windows setting.
    #[default]
    System,
    QuillDark,
    QuillLight,
    Light,
    Dark,
    SolarizedLight,
    SolarizedDark,
    Nord,
    Monokai,
    Sepia,
}

impl Theme {
    pub const ALL: [Theme; 10] = [
        Theme::System,
        Theme::QuillDark,
        Theme::QuillLight,
        Theme::Light,
        Theme::Dark,
        Theme::SolarizedLight,
        Theme::SolarizedDark,
        Theme::Nord,
        Theme::Monokai,
        Theme::Sepia,
    ];

    /// The value in config.toml.
    pub fn key(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::QuillDark => "quill-dark",
            Theme::QuillLight => "quill-light",
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::SolarizedLight => "solarized-light",
            Theme::SolarizedDark => "solarized-dark",
            Theme::Nord => "nord",
            Theme::Monokai => "monokai",
            Theme::Sepia => "sepia",
        }
    }

    /// The name in the settings window. `system` is the only one with a translation.
    pub fn label(self, system: &'static str) -> &'static str {
        match self {
            Theme::System => system,
            Theme::QuillDark => "Quill Dark",
            Theme::QuillLight => "Quill Light",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::SolarizedLight => "Solarized Light",
            Theme::SolarizedDark => "Solarized Dark",
            Theme::Nord => "Nord",
            Theme::Monokai => "Monokai",
            Theme::Sepia => "Sepia",
        }
    }
}

/// The colour roles of a Quill scheme, named after the editor's chrome.
struct Scheme {
    window: u32,
    text: u32,
    text_dim: u32,
    /// Buttons and other controls at rest.
    tab_inactive: u32,
    /// Text fields and the list background.
    tab_active: u32,
    accent: u32,
    line: u32,
    hover: u32,
    hover_line: u32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[rustfmt::skip]
fn scheme(theme: Theme) -> Option<Scheme> {
    let s = |window, text, text_dim, tab_inactive, tab_active, accent, line, hover, hover_line| {
        Some(Scheme { window, text, text_dim, tab_inactive, tab_active, accent, line, hover, hover_line })
    };
    match theme {
        Theme::System => None,
        Theme::QuillDark => s(0x16161A, 0xF2F2F2, 0xB8B8C0, 0x1E1E24, 0x2A2A31, 0xFDCF1A, 0x2E2E36, 0x3A3A44, 0xFDCF1A),
        Theme::QuillLight => s(0xF3F3F0, 0x16161A, 0x6A6A6A, 0xE6E6E1, 0xFFFFFF, 0xFDCF1A, 0xD0D0CA, 0xFFF6CC, 0xE5B800),
        Theme::Light => s(0xF3F3F3, 0x202020, 0x505050, 0xE2E2E2, 0xFFFFFF, 0x2F6FDB, 0xC4C4C4, 0xDCE8F8, 0x8FB4EA),
        Theme::Dark => s(0x2D2D30, 0xE0E0E0, 0xA0A0A0, 0x3C3C40, 0x1F1F1F, 0x4C9AFF, 0x555558, 0x3E4A58, 0x5080B0),
        Theme::SolarizedLight => s(0xEEE8D5, 0x586E75, 0x839496, 0xDDD6C1, 0xFDF6E3, 0xB58900, 0xC9C2AD, 0xE0DCC8, 0x93A1A1),
        Theme::SolarizedDark => s(0x073642, 0x93A1A1, 0x839496, 0x0A3F4C, 0x002B36, 0xB58900, 0x1D4F5C, 0x0E4B5A, 0x268BD2),
        Theme::Nord => s(0x3B4252, 0xECEFF4, 0xD8DEE9, 0x434C5E, 0x2E3440, 0x88C0D0, 0x4C566A, 0x4C566A, 0x88C0D0),
        Theme::Monokai => s(0x1E1F1C, 0xF8F8F2, 0xA0A090, 0x34352F, 0x272822, 0xF92672, 0x45463F, 0x3E3D32, 0xA6E22E),
        Theme::Sepia => s(0xE8DDC2, 0x433422, 0x6B5A40, 0xD6C8A6, 0xF4ECD8, 0xB5762B, 0xBFAF8C, 0xDCCDA8, 0xB5762B),
    }
}

fn visuals(scheme: &Scheme) -> egui::Visuals {
    let window = rgb(scheme.window);
    let text = rgb(scheme.text);
    let accent = rgb(scheme.accent);
    let mut v = if window.intensity() < 0.5 {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.override_text_color = Some(text);
    v.weak_text_color = Some(rgb(scheme.text_dim));
    v.panel_fill = window;
    v.window_fill = window;
    v.window_stroke = Stroke::new(1.0, rgb(scheme.line));
    v.extreme_bg_color = rgb(scheme.tab_active);
    // Every other list row, a small step from the list background towards the text.
    v.faint_bg_color = rgb(scheme.tab_active).lerp_to_gamma(text, 0.04);
    v.hyperlink_color = accent;
    // Text on the accent needs contrast: dark on Quill yellow, white on blue.
    let on_accent = if accent.intensity() > 0.6 {
        rgb(0x16161A)
    } else {
        Color32::WHITE
    };
    v.selection.bg_fill = accent;
    v.selection.stroke = Stroke::new(1.0, on_accent);
    let line = Stroke::new(1.0, rgb(scheme.line));
    v.widgets.noninteractive.bg_stroke = line;
    v.widgets.noninteractive.bg_fill = window;
    for widget in [&mut v.widgets.inactive, &mut v.widgets.open] {
        widget.bg_fill = rgb(scheme.tab_inactive);
        widget.weak_bg_fill = rgb(scheme.tab_inactive);
        widget.bg_stroke = line;
    }
    v.widgets.hovered.bg_fill = rgb(scheme.hover);
    v.widgets.hovered.weak_bg_fill = rgb(scheme.hover);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, rgb(scheme.hover_line));
    // The active foreground stays as egui sets it: strong text such as the
    // column headers takes its colour, so it must contrast with the window.
    v.widgets.active.bg_fill = accent;
    v.widgets.active.weak_bg_fill = accent;
    v
}

/// Sets the colours of the whole window from the next frame on.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    let quill = |theme| scheme(theme).map(|scheme| visuals(&scheme));
    let (preference, dark, light) = match quill(theme) {
        Some(chosen) if chosen.dark_mode => (egui::ThemePreference::Dark, chosen.clone(), chosen),
        Some(chosen) => (egui::ThemePreference::Light, chosen.clone(), chosen),
        None => (
            egui::ThemePreference::System,
            quill(Theme::QuillDark).unwrap_or_else(egui::Visuals::dark),
            quill(Theme::QuillLight).unwrap_or_else(egui::Visuals::light),
        ),
    };
    ctx.set_visuals_of(egui::Theme::Dark, dark);
    ctx.set_visuals_of(egui::Theme::Light, light);
    ctx.set_theme(preference);
}
