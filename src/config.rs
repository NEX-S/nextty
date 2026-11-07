use crate::error::{Error, Result};
use std::{borrow::Cow, io::ErrorKind, path::PathBuf};

pub(crate) const DEFAULT_HISTORY_LIMIT: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabStyle {
    #[default]
    Angled,
    Flat,
}

#[derive(Clone, Debug, PartialEq)]
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

    pub fn parse(text: &str) -> Result<Self> {
        let mut config = Self::default();
        let mut seen = smallvec::SmallVec::<[&str; 10]>::new();
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| Error::msg("expected key = value"))?;
            let key = key.trim();
            if seen.contains(&key) {
                return Err(Error::msg(format!("duplicate configuration field {key:?}")));
            }
            let value = value.trim();
            let (value, quoted) = if let Some(quote @ ('\'' | '"')) = value.chars().next() {
                let rest = &value[1..];
                let end = rest
                    .find(quote)
                    .ok_or_else(|| Error::msg("unterminated string"))?;
                let trailing = rest[end + 1..].trim();
                if !trailing.is_empty() && !trailing.starts_with('#') {
                    return Err(Error::msg("expected a comment after the value"));
                }
                (&rest[..end], true)
            } else {
                (value.split('#').next().unwrap().trim(), false)
            };
            let string = || {
                if quoted {
                    Ok(value)
                } else {
                    Err(Error::msg("expected a quoted string"))
                }
            };
            let number = || {
                if quoted {
                    return Err(Error::msg("expected a number"));
                }
                value
                    .replace('_', "")
                    .parse::<f32>()
                    .map_err(|error| Error::context(key, error))
            };
            match key {
                "font_family" => config.font_family = string()?.to_owned().into(),
                "base_url" => config.base_url = string()?.to_owned(),
                "model" => config.model = string()?.to_owned(),
                "font_size_px" => config.font_size_px = number()?,
                "background_opacity" => config.background_opacity = number()?,
                "tab_bar_opacity" => config.tab_bar_opacity = number()?,
                "tab_style" => {
                    config.tab_style = match string()? {
                        "angled" => TabStyle::Angled,
                        "flat" => TabStyle::Flat,
                        _ => return Err(Error::msg("tab_style must be angled or flat")),
                    }
                }
                "history_limit_bytes" => {
                    if quoted {
                        return Err(Error::msg("expected an integer"));
                    }
                    config.history_limit_bytes = value
                        .replace('_', "")
                        .parse()
                        .map_err(|error| Error::context(key, error))?;
                }
                "foreground" | "background" => {
                    let color = string()?;
                    let digits = color
                        .strip_prefix('#')
                        .filter(|digits| {
                            digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                        .ok_or_else(|| Error::msg("color must be a string in #RRGGBB format"))?;
                    let color = u32::from_str_radix(digits, 16).unwrap();
                    if key == "foreground" {
                        config.foreground = color;
                    } else {
                        config.background = color;
                    }
                }
                _ => return Err(Error::msg(format!("unknown configuration field {key:?}"))),
            }
            seen.push(key);
        }
        if config.font_family.trim().is_empty() {
            return Err(Error::msg("font_family must not be empty"));
        }
        if config.history_limit_bytes == 0 {
            return Err(Error::msg("history_limit_bytes must be greater than zero"));
        }
        if !config.font_size_px.is_finite() || config.font_size_px <= 0. {
            return Err(Error::msg(
                "font_size_px must be finite and greater than zero",
            ));
        }
        if !(0. ..=1.).contains(&config.background_opacity) {
            return Err(Error::msg("background_opacity must be between 0.0 and 1.0"));
        }
        if !(0. ..=1.).contains(&config.tab_bar_opacity) {
            return Err(Error::msg("tab_bar_opacity must be between 0.0 and 1.0"));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_configuration() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("\u{feff}").unwrap(), Config::default());
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
    fn tab_bar_opacity_is_independent_and_validated() {
        for opacity in [0., 0.5, 1.] {
            let config = Config::parse(&format!("tab_bar_opacity = {opacity}")).unwrap();
            assert_eq!(config.tab_bar_opacity, opacity);
            assert_eq!(config.background_opacity, 1.);
        }
        let config = Config::parse("background_opacity = 0.25\ntab_bar_opacity = 0.75").unwrap();
        assert_eq!(config.background_opacity, 0.25);
        assert_eq!(config.tab_bar_opacity, 0.75);
        for value in ["-0.1", "1.1", "nan", "inf", "'0.5'", "true"] {
            assert!(Config::parse(&format!("tab_bar_opacity = {value}")).is_err());
        }
        assert!(Config::parse("tab_bar_opacity = 0.7\ntab_bar_opacity = 1").is_err());
    }

    #[test]
    fn invalid_configuration_is_reported() {
        for text in [
            "font_family = ' '",
            "font_size_px = 0",
            "font_size_px = nan",
            "font_size_px = inf",
            "background_opacity = -0.1",
            "background_opacity = 1.1",
            "background_opacity = nan",
            "background = 42",
            "background = '#12345'",
            "background = '#+12345'",
            "background = '#zz0000'",
            "background = 'red'",
            "unknown_option = true",
            "background_blur = true",
            "font_size_px = 1\nfont_size_px = 2",
            "history_limit_bytes = 0",
            "history_limit_bytes = 1.5",
            "history_limit_bytes = -1",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
        assert_eq!(
            Config::parse("history_limit_bytes = 1_073_741_824")
                .unwrap()
                .history_limit_bytes,
            1 << 30
        );
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
