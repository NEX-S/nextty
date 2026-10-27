use std::{error::Error as StdError, fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Message(String),
    Context(String, Box<dyn StdError + Send + Sync>),
    Io(io::Error),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn context(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Context(message.into(), Box::new(source))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut source = match self {
            Self::Message(message) => return f.write_str(message),
            Self::Context(message, source) => {
                f.write_str(message)?;
                Some(source.as_ref() as &(dyn StdError + 'static))
            }
            Self::Io(error) => {
                write!(f, "{error}")?;
                error.source()
            }
        };
        if f.alternate() {
            while let Some(error) = source {
                if let Some(error) = error.downcast_ref::<Self>() {
                    return write!(f, ": {error:#}");
                }
                write!(f, ": {error}")?;
                source = error.source();
            }
        }
        Ok(())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Message(_) => None,
            Self::Context(_, source) => Some(source.as_ref()),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_preserve_io_error_and_display_full_chain() {
        let error = Error::context(
            "load config",
            Error::context("read file", io::Error::from(io::ErrorKind::NotFound)),
        );
        assert_eq!(error.to_string(), "load config");
        assert!(format!("{error:#}").starts_with("load config: read file: "));
        assert_eq!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn converted_io_error_is_displayed_once_and_keeps_its_source() {
        let error = Error::from(io::Error::other("workspace socket closed"));
        assert_eq!(error.to_string(), "workspace socket closed");
        assert_eq!(format!("{error:#}"), "workspace socket closed");
        assert!(error.source().unwrap().is::<io::Error>());

        let error = Error::context("connect workspace server", error);
        assert_eq!(
            format!("{error:#}"),
            "connect workspace server: workspace socket closed"
        );
    }
}
