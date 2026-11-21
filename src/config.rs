use std::{
    borrow::Cow,
    io::{self, ErrorKind},
    path::PathBuf,
};

const DEFAULT_HISTORY_LIMIT: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabStyle {
    #[default]
    Angled,
    Flat,
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub font_family: Cow<'static, str>,
    pub font_size_px: f32,
    pub foreground: u32,
    pub background: u32,
    pub background_opacity: f32,
    pub tab_bar_opacity: f32,
    pub tab_style: TabStyle,
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
            tab_style: TabStyle::Angled,
            history_limit_bytes: DEFAULT_HISTORY_LIMIT,
            base_url: String::new(),
            model: String::new(),
        }
    }
}

pub(crate) fn cache_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("nextty"))
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> io::Result<Self> {
        let explicit = path.is_some();
        let path = match path {
            Some(path) => path,
            None => match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
                Some(root) => PathBuf::from(root),
                None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                    io::Error::other("HOME is not set; specify a configuration with --config PATH")
                })?)
                .join(".config"),
            }
            .join("nextty/config.toml"),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if !explicit && error.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("read {}: {error}", path.display()),
                ));
            }
        };
        Self::parse(&text).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("configuration {}: {error}", path.display()),
            )
        })
    }

    pub fn parse(text: &str) -> io::Result<Self> {
        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| io::Error::other("expected key = value"))?;
            let (key, value) = (key.trim(), value.trim());
            let (value, quoted) = if let Some(quote @ ('\'' | '"')) = value.chars().next() {
                let end = value[1..]
                    .find(quote)
                    .ok_or_else(|| io::Error::other("unterminated string"))?;
                (&value[1..=end], true)
            } else {
                (value.split('#').next().unwrap().trim(), false)
            };
            let string = || {
                if quoted {
                    Ok(value)
                } else {
                    Err(io::Error::other("expected a quoted string"))
                }
            };
            match key {
                "font_family" => config.font_family = string()?.to_owned().into(),
                "base_url" => config.base_url = string()?.to_owned(),
                "model" => config.model = string()?.to_owned(),
                "font_size_px" => config.font_size_px = number(key, value)?,
                "background_opacity" => config.background_opacity = number(key, value)?,
                "tab_bar_opacity" => config.tab_bar_opacity = number(key, value)?,
                "history_limit_bytes" => config.history_limit_bytes = number(key, value)?,
                "tab_style" => {
                    config.tab_style = match string()? {
                        "angled" => TabStyle::Angled,
                        "flat" => TabStyle::Flat,
                        _ => return Err(io::Error::other("tab_style must be angled or flat")),
                    }
                }
                "foreground" | "background" => {
                    let color = string()?
                        .strip_prefix('#')
                        .filter(|digits| digits.len() == 6)
                        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                        .ok_or_else(|| {
                            io::Error::other("color must be a string in #RRGGBB format")
                        })?;
                    if key == "foreground" {
                        config.foreground = color;
                    } else {
                        config.background = color;
                    }
                }
                _ => {
                    return Err(io::Error::other(format!(
                        "unknown configuration field {key:?}"
                    )));
                }
            }
        }
        Ok(config)
    }
}

fn number<T: std::str::FromStr<Err: std::fmt::Display>>(key: &str, value: &str) -> io::Result<T> {
    value
        .replace('_', "")
        .parse()
        .map_err(|error| io::Error::other(format!("{key}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_configuration() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let config = Config::parse(
            r##"font_family = "monospace"
font_size_px = 20
foreground = "#aBcDeF"
background_opacity = 0.75
base_url = "http://localhost:8080/anthropic"
model = "deepseek-flash""##,
        )
        .unwrap();
        assert_eq!(config.font_family, "monospace");
        assert_eq!(config.font_size_px, 20.);
        assert_eq!(config.foreground, 0xabcdef);
        assert_eq!(config.background, Config::default().background);
        assert_eq!(config.background_opacity, 0.75);
        assert_eq!(config.tab_bar_opacity, 0.7);
        assert_eq!(config.base_url, "http://localhost:8080/anthropic");
        assert_eq!(config.model, "deepseek-flash");
    }

    #[test]
    fn example_configuration_and_inline_comments() {
        let config = Config::parse(include_str!("../nextty.example.toml")).unwrap();
        assert_eq!(config.font_family, Config::default().font_family);
        assert_eq!(config.history_limit_bytes, DEFAULT_HISTORY_LIMIT);
        let config = Config::parse(
            "font_family = 'mono space' # font\r\nforeground = \"#abcdef\" # color\nbase_url = \"https://example.test/#fragment\"\n",
        ).unwrap();
        assert_eq!(config.font_family, "mono space");
        assert_eq!(config.foreground, 0xabcdef);
        assert_eq!(config.base_url, "https://example.test/#fragment");
        assert_eq!(
            Config::parse("history_limit_bytes = 1_073_741_824")
                .unwrap()
                .history_limit_bytes,
            1 << 30
        );
    }

    #[test]
    fn explicit_missing_path_preserves_io_error() {
        let path = std::env::temp_dir()
            .join(format!("nextty-config-missing-{}", std::process::id()))
            .join("config.toml");
        let error = Config::load(Some(path.clone())).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.to_string().contains(&path.display().to_string()));
    }
}
