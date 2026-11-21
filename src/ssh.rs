use std::{io, net::Ipv6Addr, process::Command};

#[derive(Debug, PartialEq)]
pub struct RemoteTarget {
    destination: String,
    port: Option<u16>,
}

impl RemoteTarget {
    pub fn recognizes(value: &str) -> bool {
        value.contains(['@', ':'])
    }

    pub fn parse(value: &str) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote target must be [user@]host[:port] or ssh://[user@]host[:port]; use brackets around IPv6 with a port",
            )
        };
        let address = value.strip_prefix("ssh://").unwrap_or(value);
        let (user, host_port) = address.split_at(address.find('@').map_or(0, |at| at + 1));
        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            let (host, suffix) = rest.split_once(']').ok_or_else(invalid)?;
            (host, suffix.strip_prefix(':'))
        } else if host_port.parse::<Ipv6Addr>().is_ok() {
            (host_port, None)
        } else {
            host_port
                .split_once(':')
                .map_or((host_port, None), |(host, port)| (host, Some(port)))
        };
        Ok(Self {
            destination: format!("{user}{host}"),
            port: port
                .map(|port| port.parse().map_err(|_| invalid()))
                .transpose()?,
        })
    }

    pub fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{} (port {port})", self.destination),
            None => self.destination.clone(),
        }
    }

    /// `new` makes the remote bridge start the workspace server and `list` makes it end quietly
    /// when there is none; other commands connect to a running server only.
    pub fn command(&self, bridge: &str) -> Command {
        let mut command = Command::new("ssh");
        command.args(["-T", "-oRemoteCommand=none"]);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        command
            .arg("--")
            .arg(&self.destination)
            .arg(format!("nextty --bridge {bridge}"));
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
    fn ssh_arguments_do_not_include_session_or_program_shell_text() {
        let command = RemoteTarget::parse("nex@host:2222")
            .unwrap()
            .command("attach");
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
                "nextty --bridge attach"
            ]
        );
    }
}
