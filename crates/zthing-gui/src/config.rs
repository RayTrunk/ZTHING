use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::theme::Theme;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub drives: Vec<String>,
    pub exclude: Vec<String>,
    pub hotkey: String,
    pub theme: Theme,
    pub preview: bool,
    pub http: Http,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    pub enabled: bool,
    pub port: u16,
    pub token: String,
}

impl Default for Http {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8732,
            token: String::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            drives: Vec::new(),
            exclude: Vec::new(),
            hotkey: "Alt+Space".to_owned(),
            theme: Theme::System,
            preview: true,
            http: Http::default(),
        }
    }
}

impl Config {
    /// The whole file with an explanation above every setting. Written on the
    /// first start and by the settings window, so saving keeps the explanations.
    pub fn render(&self) -> String {
        let string = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let list = |items: &[String]| {
            let items: Vec<String> = items.iter().map(|item| string(item)).collect();
            format!("[{}]", items.join(", "))
        };
        format!(
            r#"# ZTHING settings. The settings window writes this file; changes made by hand
# take effect at the next start.

# Drive letters to search, e.g. ["C", "D"]. Empty: every drive the service indexes.
drives = {drives}

# Folders whose contents are hidden from results, e.g. ['C:\Windows\WinSxS']
exclude = {exclude}

# Shortcut that shows the window, e.g. "Alt+Space" or "Ctrl+Shift+F"
hotkey = {hotkey}

# system (follows Windows), quill-dark, quill-light, light, dark,
# solarized-light, solarized-dark, nord, monokai or sepia
theme = "{theme}"

# Preview pane right of the list. F3 shows or hides it while ZTHING runs.
preview = {preview}

# HTTP API for other programs: GET /search and GET /status on 127.0.0.1 only.
# Requests send the token as "Authorization: Bearer <token>" or as ?token=<token>.
# It runs while this window runs, with the rights of the logged-on user.
[http]
enabled = {enabled}
port = {port}
token = {token}
"#,
            drives = list(&self.drives),
            exclude = list(&self.exclude),
            hotkey = string(&self.hotkey),
            theme = self.theme.key(),
            preview = self.preview,
            enabled = self.http.enabled,
            port = self.http.port,
            token = string(&self.http.token),
        )
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.render())
    }

    /// The service indexes every drive, so the chosen drives and excluded
    /// folders become terms in front of each query. The trailing backslash
    /// hides a folder's contents but not folders that merely start with the same name.
    pub fn query_prefix(&self) -> String {
        let drives = self
            .drives
            .iter()
            .filter_map(|drive| drive.trim().chars().next())
            .map(|drive| format!("path:\"{}:\\\"", drive.to_ascii_uppercase()))
            .collect::<Vec<_>>()
            .join("|");
        let exclusions: String = self
            .exclude
            .iter()
            .map(|folder| format!("!path:\"{}\\\" ", folder.trim().trim_end_matches('\\')))
            .collect();
        if drives.is_empty() {
            exclusions
        } else {
            format!("{drives} {exclusions}")
        }
    }
}

/// Reads the config and creates it with defaults on first start. An invalid
/// file falls back to the defaults and returns the problem for the status line.
pub fn load(path: &Path) -> (Config, Option<String>) {
    let problem = match fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(config) => return (config, None),
            Err(e) => format!("{}: {e}", path.display()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => match Config::default().save(path) {
            Ok(()) => return (Config::default(), None),
            Err(e) => format!("{}: {e}", path.display()),
        },
        Err(e) => format!("{}: {e}", path.display()),
    };
    (Config::default(), Some(problem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_files_read_back_unchanged() {
        assert_eq!(
            toml::from_str::<Config>(&Config::default().render()).ok(),
            Some(Config::default())
        );
        let config = Config {
            drives: vec!["C".to_owned(), "E".to_owned()],
            exclude: vec![r"C:\Windows\WinSxS".to_owned(), r#"D:\a "b"\c"#.to_owned()],
            hotkey: "Ctrl+Shift+F".to_owned(),
            theme: Theme::SolarizedDark,
            preview: false,
            http: Http {
                enabled: true,
                port: 9000,
                token: "geheim\"1".to_owned(),
            },
        };
        assert_eq!(
            toml::from_str::<Config>(&config.render()).ok(),
            Some(config)
        );
    }

    #[test]
    fn partial_files_keep_the_other_defaults() {
        let config: Config = toml::from_str("drives = [\"D\"]\ntheme = \"dark\"").unwrap();
        assert_eq!(config.drives, ["D"]);
        assert_eq!(config.theme, Theme::Dark);
        assert_eq!(config.hotkey, "Alt+Space");
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
        let config: Config = toml::from_str("[http]\nenabled = true\ntoken = \"t\"").unwrap();
        assert_eq!((config.http.enabled, config.http.port), (true, 8732));
    }

    #[test]
    fn drives_and_exclusions_become_query_terms() {
        let config = Config {
            drives: vec!["c".to_owned(), "D:".to_owned()],
            exclude: vec![r"C:\Temp\".to_owned()],
            ..Config::default()
        };
        assert_eq!(
            config.query_prefix(),
            r#"path:"C:\"|path:"D:\" !path:"C:\Temp\" "#
        );
        assert_eq!(Config::default().query_prefix(), "");
    }
}
