use crate::error::{Error, Result};
use std::{io::ErrorKind, path::PathBuf};

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

    pub fn parse(text: &str) -> Result<Self> {
        let mut parser = Parser {
            text,
            offset: usize::from(text.starts_with('\u{feff}')) * 3,
        };
        let mut config = Self::default();
        let mut seen = Vec::new();
        while parser.line_start()? {
            let key = parser.key()?;
            parser.space();
            parser.expect('=')?;
            parser.space();
            if seen.contains(&key) {
                return parser.fail(format!("duplicate configuration field {key:?}"));
            }
            match key.as_str() {
                "font_family" => config.font_family = parser.string(true)?,
                "font_size_px" => config.font_size_px = parser.number()?,
                "foreground" => config.foreground = parser.color()?,
                "background" => config.background = parser.color()?,
                "background_opacity" => config.background_opacity = parser.number()?,
                "history_limit_bytes" => config.history_limit_bytes = parser.integer()?,
                "tab_bar_opacity" => config.tab_bar_opacity = parser.number()?,
                "base_url" => config.base_url = parser.string(true)?,
                "model" => config.model = parser.string(true)?,
                _ => return parser.fail(format!("unknown configuration field {key:?}")),
            }
            seen.push(key);
            parser.space();
            parser.comment()?;
            if parser.peek().is_some() && !parser.newline() {
                return parser.fail("expected a newline after the configuration value");
            }
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

// Only these scalar fields can be valid Config values. Tables, arrays and dates
// cannot deserialize into them, so no general TOML value tree is needed.
struct Parser<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> Parser<'a> {
    fn fail<T>(&self, message: impl std::fmt::Display) -> Result<T> {
        let prefix = &self.text[..self.offset];
        let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
        let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
        Err(Error::msg(format!(
            "line {line}, column {column}: {message}"
        )))
    }

    fn peek(&self) -> Option<char> {
        self.text[self.offset..].chars().next()
    }

    fn take(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        if self.peek() != Some(ch) {
            return self.fail(format!("expected {ch:?}"));
        }
        self.take();
        Ok(())
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.take();
        }
    }

    fn newline(&mut self) -> bool {
        if self.text[self.offset..].starts_with("\r\n") {
            self.offset += 2;
            true
        } else if self.peek() == Some('\n') {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn comment(&mut self) -> Result<()> {
        if self.peek() == Some('#') {
            self.take();
            while let Some(ch) = self.peek() {
                if matches!(ch, '\r' | '\n') {
                    break;
                }
                if ch.is_ascii_control() && ch != '\t' {
                    return self.fail("control character in comment");
                }
                self.take();
            }
        }
        Ok(())
    }

    fn line_start(&mut self) -> Result<bool> {
        loop {
            self.space();
            self.comment()?;
            if !self.newline() {
                return Ok(self.peek().is_some());
            }
        }
    }

    fn key(&mut self) -> Result<String> {
        if matches!(self.peek(), Some('\'' | '"')) {
            return self.string(false);
        }
        let start = self.offset;
        while self
            .peek()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        {
            self.take();
        }
        if start == self.offset {
            return self.fail("expected a configuration field name");
        }
        Ok(self.text[start..self.offset].to_owned())
    }

    fn string(&mut self, allow_multiline: bool) -> Result<String> {
        let quote = match self.take() {
            Some(ch @ ('\'' | '"')) => ch,
            _ => return self.fail("expected a string"),
        };
        let triple = self.text.as_bytes()[self.offset..].starts_with(&[quote as u8; 2]);
        if triple && !allow_multiline {
            return self.fail("multiline keys are not allowed");
        }
        if triple {
            self.offset += 2;
            self.newline();
        }
        let mut value = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return self.fail("unterminated string");
            };
            if ch == quote {
                let count = self.text[self.offset..]
                    .chars()
                    .take_while(|&ch| ch == quote)
                    .count();
                if !triple {
                    self.take();
                    return Ok(value);
                }
                if count >= 3 {
                    if count > 5 {
                        return self.fail("too many closing quotes");
                    }
                    value.extend(std::iter::repeat_n(quote, count - 3));
                    self.offset += count;
                    return Ok(value);
                }
                value.extend(std::iter::repeat_n(quote, count));
                self.offset += count;
                continue;
            }
            if triple {
                let start = self.offset;
                if self.newline() {
                    value.push_str(&self.text[start..self.offset]);
                    continue;
                }
            }
            self.take();
            if ch.is_ascii_control() && ch != '\t' {
                return self.fail("control character in string");
            }
            if ch != '\\' || quote == '\'' {
                value.push(ch);
                continue;
            }
            if triple && matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
                self.space();
                if !self.newline() {
                    return self.fail("string continuation requires a newline");
                }
                loop {
                    self.space();
                    if !self.newline() {
                        break;
                    }
                }
                continue;
            }
            let escaped = match self.take() {
                Some('b') => '\u{8}',
                Some('t') => '\t',
                Some('n') => '\n',
                Some('f') => '\u{c}',
                Some('r') => '\r',
                Some('e') => '\u{1b}',
                Some('"') => '"',
                Some('\\') => '\\',
                Some(kind @ ('x' | 'u' | 'U')) => {
                    let count = match kind {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let mut scalar = 0u32;
                    for _ in 0..count {
                        let Some(digit) = self.take().and_then(|ch| ch.to_digit(16)) else {
                            return self.fail("invalid Unicode escape");
                        };
                        scalar = (scalar << 4) | digit;
                    }
                    let Some(ch) = char::from_u32(scalar) else {
                        return self.fail("Unicode escape is not a scalar value");
                    };
                    ch
                }
                _ => return self.fail("invalid string escape"),
            };
            value.push(escaped);
        }
    }

    fn token(&mut self) -> &'a str {
        let start = self.offset;
        while self
            .peek()
            .is_some_and(|ch| !matches!(ch, ' ' | '\t' | '\r' | '\n' | '#'))
        {
            self.take();
        }
        &self.text[start..self.offset]
    }

    fn color(&mut self) -> Result<u32> {
        let value = self.string(true)?;
        let Some(digits) = value.strip_prefix('#').filter(|digits| {
            digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) else {
            return self.fail("color must be a string in #RRGGBB format");
        };
        Ok(u32::from_str_radix(digits, 16).unwrap())
    }

    fn number(&mut self) -> Result<f32> {
        let token = self.token();
        let invalid = || Error::msg(format!("invalid configuration number {token:?}"));
        let unsigned = token.strip_prefix(['+', '-']).unwrap_or(token);
        if matches!(unsigned, "inf" | "nan") {
            return Ok(token.parse::<f64>().map_err(|_| invalid())? as f32);
        }
        for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
            if let Some(digits) = token.strip_prefix(prefix) {
                if !digits_valid(digits, radix) {
                    return Err(invalid());
                }
                return u64::from_str_radix(&digits.replace('_', ""), radix)
                    .map(|value| value as f32)
                    .map_err(|_| invalid());
            }
        }
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
            None => (unsigned, None),
        };
        let (integer, fraction) = match mantissa.split_once('.') {
            Some((integer, fraction)) => (integer, Some(fraction)),
            None => (mantissa, None),
        };
        if !digits_valid(integer, 10)
            || integer.starts_with('0') && integer.len() != 1
            || fraction.is_some_and(|digits| !digits_valid(digits, 10))
            || exponent.is_some_and(|digits| {
                !digits_valid(digits.strip_prefix(['+', '-']).unwrap_or(digits), 10)
            })
        {
            return Err(invalid());
        }
        let cleaned = token.replace('_', "");
        if fraction.is_some() || exponent.is_some() {
            cleaned
                .parse::<f64>()
                .map(|value| value as f32)
                .map_err(|_| invalid())
        } else if token.starts_with('-') {
            cleaned
                .parse::<i64>()
                .map(|value| value as f32)
                .map_err(|_| invalid())
        } else {
            cleaned
                .parse::<u64>()
                .map(|value| value as f32)
                .map_err(|_| invalid())
        }
    }

    fn integer(&mut self) -> Result<u64> {
        let token = self.token();
        let (digits, radix) = if let Some(digits) = token.strip_prefix("0x") {
            (digits, 16)
        } else if let Some(digits) = token.strip_prefix("0o") {
            (digits, 8)
        } else if let Some(digits) = token.strip_prefix("0b") {
            (digits, 2)
        } else {
            (token.strip_prefix('+').unwrap_or(token), 10)
        };
        if !digits_valid(digits, radix)
            || radix == 10 && digits.starts_with('0') && digits.len() != 1
        {
            return Err(Error::msg(format!(
                "invalid configuration integer {token:?}"
            )));
        }
        u64::from_str_radix(&digits.replace('_', ""), radix)
            .map_err(|_| Error::msg(format!("configuration integer out of range {token:?}")))
    }
}

fn digits_valid(digits: &str, radix: u32) -> bool {
    !digits.is_empty()
        && digits.split('_').all(|part| {
            !part.is_empty() && part.chars().all(|ch| ch.is_ascii() && ch.is_digit(radix))
        })
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
    fn quoted_keys_multiline_strings_and_escapes() {
        let config = Config::parse("\"font\\u005ffamily\" = \"\"\"\r\nmono\\  \r\n   space\"\"\"\nforeground = \"\\x23aBcDeF\"\n").unwrap();
        assert_eq!(config.font_family, "monospace");
        assert_eq!(config.foreground, 0xabcdef);
        assert_eq!(
            Config::parse("font_family = '''\nmono\r\nspace'''\n")
                .unwrap()
                .font_family,
            "mono\r\nspace"
        );
        assert_eq!(
            Config::parse("font_family = \"mono\\u0000space\"")
                .unwrap()
                .font_family,
            "mono\0space"
        );
    }

    #[test]
    fn numeric_boundaries() {
        for number in [
            "0b1_0000",
            "0o20",
            "0x10",
            "1_6",
            "+16",
            "1.6e+1",
            "9223372036854775807",
            "9223372036854775808",
            "18446744073709551615",
        ] {
            assert!(
                Config::parse(&format!("font_size_px = {number}")).is_ok(),
                "{number}"
            );
        }
        for number in [
            "18446744073709551616",
            "016",
            "0_1",
            "-0x10",
            "16_",
            "1__6",
            "1e-99",
            "1e99",
            "1.",
            ".1",
            "1e",
            "1e_1",
            "0x_10",
            "0b2",
            "1e1.0",
        ] {
            assert!(
                Config::parse(&format!("font_size_px = {number}")).is_err(),
                "{number}"
            );
        }
        assert_eq!(
            Config::parse("background_opacity = 1.00000001")
                .unwrap()
                .background_opacity,
            1.
        );
        assert!(Config::parse("background_opacity = 1.0000001").is_err());
    }

    #[test]
    fn rejects_wrong_types_syntax_and_control_characters() {
        for text in [
            "[font_family]",
            "font_family.x = 'x'",
            "font_family = ['x']",
            "font_family = {x='x'}",
            "font_size_px = 16 other = 1",
            "font_family = \"\\q\"",
            "font_family = \"\\uD800\"",
            "font_family = 'mono\nspace'",
            "font_size_px = 16\rbackground_opacity = 0.5",
            "# comment\0",
            "font_family = 'mono\0space'",
            "font_size_px = 16\n\"font\\u005fsize_px\" = 20",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text:?}");
        }
        assert!(
            Config::parse("font_size_px = 16 # comment\r\nbackground_opacity = 0.5\r\n").is_ok()
        );
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
