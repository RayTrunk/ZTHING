use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// Written on first start, so the user finds every setting with an explanation.
pub const DEFAULT_FILE: &str = r#"# ZTHING settings. Changes take effect at the next start.

# Drive letters to search, e.g. ["C", "D"]. Empty: every drive the service indexes.
drives = []

# Folders whose contents are hidden from results, e.g. ['C:\Windows\WinSxS']
exclude = []

# Shortcut that shows the window, e.g. "Alt+Space" or "Ctrl+Shift+F"
hotkey = "Alt+Space"

# system, light or dark
theme = "system"

# Preview pane right of the list. F3 shows or hides it while ZTHING runs.
preview = true

# HTTP API for other programs: GET /search and GET /status on 127.0.0.1 only.
# Requests send the token as "Authorization: Bearer <token>" or as ?token=<token>.
# It runs while this window runs, with the rights of the logged-on user.
[http]
enabled = false
port = 8732
token = ""
"#;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub drives: Vec<String>,
    pub exclude: Vec<String>,
    pub hotkey: String,
    pub theme: Theme,
    pub preview: bool,
    pub http: Http,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
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

/// Reads the config and creates it with defaults on first start. An invalid
/// file falls back to the defaults and returns the problem for the status line.
pub fn load(path: &Path) -> (Config, Option<String>) {
    let problem = match fs::read_to_string(path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(config) => return (config, None),
            Err(e) => format!("{}: {e}", path.display()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let written = path
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(path, DEFAULT_FILE));
            match written {
                Ok(()) => return (Config::default(), None),
                Err(e) => format!("{}: {e}", path.display()),
            }
        }
        Err(e) => format!("{}: {e}", path.display()),
    };
    (Config::default(), Some(problem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_the_defaults() {
        assert_eq!(
            toml::from_str::<Config>(DEFAULT_FILE).ok(),
            Some(Config::default())
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
}
