use std::{io, net::Ipv6Addr, process::Command};

#[derive(Debug, PartialEq)]
pub struct RemoteTarget {
    destination: String,
    port: Option<u16>,
}

impl RemoteTarget {
    pub fn recognizes(value: &str) -> bool {
        value.starts_with("ssh://") || value.contains(['@', ':'])
    }

    pub fn parse(value: &str) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote target must be [user@]host[:port] or ssh://[user@]host[:port]; use brackets around IPv6 with a port",
            )
        };
        let address = value.strip_prefix("ssh://").unwrap_or(value);
        let (user, host_port) = match address.split_once('@') {
            Some((user, host)) => {
                if user.is_empty()
                    || !user
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                {
                    return Err(invalid());
                }
                (Some(user), host)
            }
            None => (None, address),
        };
        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            let (host, suffix) = rest.split_once(']').ok_or_else(invalid)?;
            host.parse::<Ipv6Addr>().map_err(|_| invalid())?;
            let port = if suffix.is_empty() {
                None
            } else {
                Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
            };
            (host, port)
        } else if host_port.parse::<Ipv6Addr>().is_ok() {
            (host_port, None)
        } else {
            let (host, port) = host_port
                .split_once(':')
                .map_or((host_port, None), |(host, port)| (host, Some(port)));
            if host.is_empty()
                || host.starts_with('-')
                || !host
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                return Err(invalid());
            }
            (host, port)
        };
        let port = port
            .map(|port| {
                if port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(invalid());
                }
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        Ok(Self {
            destination: user.map_or_else(|| host.to_owned(), |user| format!("{user}@{host}")),
            port,
        })
    }

    pub fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{} (port {port})", self.destination),
            None => self.destination.clone(),
        }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args(["-T", "-oRemoteCommand=none"]);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        command.arg("--").arg(&self.destination).arg(format!(
            "nextty --bridge {}",
            nextty::workspace::BRIDGE_VERSION
        ));
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_keep_ssh_aliases_and_separate_ports() {
        for (input, destination, port) in [
            ("nex@192.0.2.1:2222", "nex@192.0.2.1", Some(2222)),
            ("devbox", "devbox", None),
            ("ssh://devbox", "devbox", None),
            ("ssh://nex@devbox:22", "nex@devbox", Some(22)),
            ("nex@[2001:db8::1]:2222", "nex@2001:db8::1", Some(2222)),
            ("[::1]", "::1", None),
            ("nex@::1", "nex@::1", None),
        ] {
            assert_eq!(
                RemoteTarget::parse(input).unwrap(),
                RemoteTarget {
                    destination: destination.into(),
                    port
                }
            );
        }
        assert!(!RemoteTarget::recognizes("project"));
        assert!(RemoteTarget::recognizes("ssh://devbox"));
        assert!(RemoteTarget::recognizes("devbox:2222"));
    }

    #[test]
    fn unsupported_addresses_and_options_are_rejected_before_ssh() {
        for input in [
            "",
            "-oProxyCommand=bad",
            "@host",
            "user@",
            "user@@host",
            "host:0",
            "host:65536",
            "host:abc",
            "host:",
            "[::1",
            "[host]:22",
            "[::1]/path",
            "ssh://host/path",
            "host;command",
            "host\ncommand",
            "host name",
        ] {
            assert!(RemoteTarget::parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn ssh_arguments_do_not_include_session_or_program_shell_text() {
        let command = RemoteTarget::parse("nex@host:2222").unwrap().command();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-T",
                "-oRemoteCommand=none",
                "-p",
                "2222",
                "--",
                "nex@host",
                &format!("nextty --bridge {}", nextty::workspace::BRIDGE_VERSION)
            ]
        );
    }
}
