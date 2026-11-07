use crate::error::{Error, Result};
use std::{fmt::Display, io::ErrorKind, path::PathBuf, str::FromStr};

pub(crate) const DEFAULT_HISTORY_LIMIT: u64 = 1 << 30;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub font_family: String,
    pub font_size_px: f32,
    pub foreground: u32,
    pub background: u32,
    pub background_opacity: f32,
    pub tab_bar_opacity: f32,
    pub history_limit_bytes: u64,
    pub base_url: String,
    pub model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: "CodeNewRoman Nerd Font Mono".into(),
            font_size_px: 17.,
            foreground: 0xcccccc,
            background: 0x202020,
            background_opacity: 1.,
            tab_bar_opacity: 0.7,
            history_limit_bytes: DEFAULT_HISTORY_LIMIT,
            base_url: String::new(),
            model: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let explicit = path.is_some();
        let path = match path {
            Some(path) => path,
            None => {
                let root = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
                    Some(path) => {
                        let path = PathBuf::from(path);
                        if !path.is_absolute() {
                            return Err(Error::msg("XDG_CONFIG_HOME must be an absolute path"));
                        }
                        path
                    }
                    None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                        Error::msg("HOME is not set; specify a configuration with --config PATH")
                    })?)
                    .join(".config"),
                };
                root.join("nextty/config.toml")
            }
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if !explicit && error.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(Error::context(format!("read {}", path.display()), error)),
        };
        Self::parse(&text)
            .map_err(|error| Error::context(format!("configuration {}", path.display()), error))
    }

    // The documented TOML subset: `key = value` lines, `#` comments, double-quoted
    // strings without escapes, and numbers with optional `_` separators.
    fn parse(text: &str) -> Result<Self> {
        let mut config = Self::default();
        for (index, line) in text.lines().enumerate() {
            config
                .set(line.trim())
                .map_err(|message| Error::msg(format!("line {}: {message}", index + 1)))?;
        }
        Ok(config)
    }

    fn set(&mut self, line: &str) -> std::result::Result<(), String> {
        if line.is_empty() || line.starts_with('#') {
            return Ok(());
        }
        let (key, value) = line.split_once('=').ok_or("expected key = value")?;
        let value = value.trim();
        match key.trim() {
            "font_family" => self.font_family = string(value)?.into(),
            "font_size_px" => self.font_size_px = number(value)?,
            "foreground" => self.foreground = color(value)?,
            "background" => self.background = color(value)?,
            "background_opacity" => self.background_opacity = number(value)?,
            "tab_bar_opacity" => self.tab_bar_opacity = number(value)?,
            "history_limit_bytes" => self.history_limit_bytes = number(value)?,
            "base_url" => self.base_url = string(value)?.into(),
            "model" => self.model = string(value)?.into(),
            key => return Err(format!("unknown configuration field {key:?}")),
        }
        Ok(())
    }
}

fn string(value: &str) -> std::result::Result<&str, String> {
    let (string, rest) = value
        .strip_prefix('"')
        .and_then(|value| value.split_once('"'))
        .ok_or("expected a double-quoted string")?;
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(string)
    } else {
        Err(format!("unexpected {rest:?} after the string"))
    }
}

fn number<T: FromStr<Err: Display>>(value: &str) -> std::result::Result<T, String> {
    let token = value.split_once('#').map_or(value, |(token, _)| token);
    token
        .trim()
        .replace('_', "")
        .parse()
        .map_err(|error: T::Err| error.to_string())
}

fn color(value: &str) -> std::result::Result<u32, String> {
    string(value)?
        .strip_prefix('#')
        .filter(|digits| digits.len() == 6)
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
        .ok_or_else(|| "expected a \"#RRGGBB\" color".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_example_parses_to_the_defaults() {
        assert_eq!(
            Config::parse(include_str!("../nextty.example.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn values_comments_and_separators() {
        let config = Config::parse(
            "# comment\r\n\
             font_family = \"mono # space\" # trailing\r\n\
             font_size_px = 20\n\
             \tforeground = \"#aBcDeF\"\n\
             background_opacity=0.75 # comment\n\
             history_limit_bytes = 1_073_741_824\n\
             base_url = \"http://localhost:8080/anthropic\"\n\
             model = \"deepseek-flash\"\n",
        )
        .unwrap();
        assert_eq!(config.font_family, "mono # space");
        assert_eq!(config.font_size_px, 20.);
        assert_eq!(config.foreground, 0xabcdef);
        assert_eq!(config.background, Config::default().background);
        assert_eq!(config.background_opacity, 0.75);
        assert_eq!(config.tab_bar_opacity, 0.7);
        assert_eq!(config.history_limit_bytes, 1 << 30);
        assert_eq!(config.base_url, "http://localhost:8080/anthropic");
        assert_eq!(config.model, "deepseek-flash");
    }

    #[test]
    fn errors_report_their_line() {
        for (text, message) in [
            ("model = \"x\"\nunknown = true", "line 2: unknown configuration field"),
            ("\n\nbackground = \"red\"", "line 3: expected a \"#RRGGBB\" color"),
            ("font_family = monospace", "line 1: expected a double-quoted string"),
            ("font_size_px = \"17\"", "line 1: invalid float literal"),
            ("history_limit_bytes = -1", "line 1: invalid digit"),
            ("font_size_px = 16 other = 1", "line 1: invalid float literal"),
            ("[font]", "line 1: expected key = value"),
        ] {
            let error = Config::parse(text).unwrap_err().to_string();
            assert!(error.starts_with(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn explicit_missing_path_preserves_io_source() {
        use std::error::Error as _;
        let path = std::env::temp_dir()
            .join(format!("nextty-config-missing-{}", std::process::id()))
            .join("config.toml");
        let error = Config::load(Some(path.clone())).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            ErrorKind::NotFound
        );
    }
}
