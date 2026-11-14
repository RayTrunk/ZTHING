/// Interface texts. The language follows the system: German for `de-*`, English otherwise.
pub struct Texts {
    pub search_hint: &'static str,
    pub name: &'static str,
    pub folder: &'static str,
    pub size: &'static str,
    pub modified: &'static str,
    pub objects: &'static str,
    pub open: &'static str,
    pub open_folder: &'static str,
    pub copy_path: &'static str,
    pub show: &'static str,
    pub quit: &'static str,
    pub indexing: &'static str,
    pub no_service: &'static str,
    pub hotkey_failed: &'static str,
}

const GERMAN: Texts = Texts {
    search_hint: "Suchen",
    name: "Name",
    folder: "Pfad",
    size: "Größe",
    modified: "Änderungsdatum",
    objects: "Objekte",
    open: "Öffnen",
    open_folder: "Ordner öffnen",
    copy_path: "Pfad kopieren",
    show: "Anzeigen",
    quit: "Beenden",
    indexing: "Laufwerke werden eingelesen …",
    no_service: "ZTHING-Dienst nicht erreichbar",
    hotkey_failed: "Tastenkürzel nicht verfügbar",
};

const ENGLISH: Texts = Texts {
    search_hint: "Search",
    name: "Name",
    folder: "Path",
    size: "Size",
    modified: "Date modified",
    objects: "objects",
    open: "Open",
    open_folder: "Open folder",
    copy_path: "Copy path",
    show: "Show",
    quit: "Quit",
    indexing: "Reading drives …",
    no_service: "ZTHING service unreachable",
    hotkey_failed: "Shortcut unavailable",
};

pub fn system() -> &'static Texts {
    match sys_locale::get_locale() {
        Some(locale) if locale.starts_with("de") => &GERMAN,
        _ => &ENGLISH,
    }
}
