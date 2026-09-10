use std::net::IpAddr;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::McpServer;
use clap::{Parser, ValueEnum};

use crate::mcp_config::{AcpMcpProvider, load_mcp_configs};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Transport {
    Stdio,
    #[value(alias = "sse", alias = "streamable-http")]
    Http,
    Ws,
}

impl Transport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Ws => "ws",
        }
    }
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "attyd",
    version = crate::VERSION,
    about = "Expose an ACP agent as a web workspace",
    trailing_var_arg = true,
    disable_help_subcommand = true
)]
pub struct Options {
    /// Bind address.
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// HTTP port.
    #[arg(short, long, default_value_t = 7331)]
    pub port: u16,

    /// Explicit browser origin for a custom domain or HTTPS reverse proxy. May be repeated.
    #[arg(long = "allowed-origin", value_parser = normalize_origin)]
    pub allowed_origins: Vec<String>,

    /// Stdio workspace default and local filesystem boundary.
    #[arg(short = 'c', long, default_value = ".", value_parser = absolute_or_resolve)]
    pub cwd: PathBuf,

    /// Agent transport.
    #[arg(short = 't', long, value_enum, default_value_t = Transport::Stdio)]
    pub transport: Transport,

    /// Additional stdio workspace root. May be repeated.
    #[arg(long = "add-dir", value_parser = absolute_or_resolve)]
    pub additional_directories: Vec<PathBuf>,

    /// Static MCP configuration file. May be repeated.
    #[arg(long = "mcp-config", value_parser = absolute_or_resolve)]
    pub mcp_configs: Vec<PathBuf>,

    /// Parsed MCP definitions passed to every ACP session.
    #[arg(skip)]
    pub mcp_servers: Vec<McpServer>,

    /// Private process definitions for MCP-over-ACP providers.
    #[arg(skip)]
    pub acp_mcp_providers: Vec<AcpMcpProvider>,

    /// Do not advertise or permit filesystem writes.
    #[arg(long)]
    pub read_only: bool,

    /// Close sessions after this many unobserved seconds: negative disables, zero closes immediately.
    /// Closing may stop Agent tasks and managed terminals; history recovery depends on the Agent.
    #[arg(long, default_value_t = 1800, allow_negative_numbers = true)]
    pub session_unobserved_timeout: i64,

    /// Agent command for stdio, or one endpoint URL for a remote transport.
    pub command: Vec<String>,
}

impl Options {
    pub fn normalized(mut self) -> Result<Self, String> {
        if self
            .command
            .first()
            .is_some_and(|argument| argument == "--")
        {
            self.command.remove(0);
        }
        match self.transport {
            Transport::Stdio => {
                if self.command.is_empty() {
                    return Err("stdio transport requires an Agent command; specify -- <agent-command> [args...]".to_string());
                }
            }
            Transport::Http | Transport::Ws => {
                if self.command.len() != 1 {
                    return Err(format!(
                        "--transport {} accepts exactly one ACP endpoint URL",
                        self.transport.as_str()
                    ));
                }
                let endpoint = self.command[0]
                    .parse::<axum::http::Uri>()
                    .map_err(|_| format!("invalid {} ACP endpoint URL", self.transport.as_str()))?;
                let expected = match self.transport {
                    Transport::Http => ["http", "https"].as_slice(),
                    Transport::Ws => ["ws", "wss"].as_slice(),
                    Transport::Stdio => unreachable!(),
                };
                if !endpoint
                    .scheme_str()
                    .is_some_and(|scheme| expected.contains(&scheme))
                {
                    return Err(format!(
                        "invalid {} ACP endpoint URL protocol",
                        self.transport.as_str()
                    ));
                }
                if !self.additional_directories.is_empty() {
                    return Err("--add-dir is only available with the stdio transport".to_string());
                }
            }
        }
        self.additional_directories.sort();
        self.additional_directories.dedup();
        self.additional_directories.retain(|path| path != &self.cwd);
        self.allowed_origins = self
            .allowed_origins
            .iter()
            .map(|origin| normalize_origin(origin))
            .collect::<Result<_, _>>()?;
        self.allowed_origins.sort();
        self.allowed_origins.dedup();
        let (mcp_servers, acp_mcp_providers) = load_mcp_configs(&self.mcp_configs)?;
        self.mcp_servers = mcp_servers;
        self.acp_mcp_providers = acp_mcp_providers;
        Ok(self)
    }
}

pub(crate) fn normalize_origin(value: &str) -> Result<String, String> {
    let invalid = || {
        "allowed origin must be an HTTP(S) scheme and authority only, without credentials, path, query, or fragment".to_string()
    };
    let (_, authority) = value.split_once("://").ok_or_else(invalid)?;
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
        || authority.is_empty()
        || authority.contains(['/', '\\', '?', '#', '@', '*'])
    {
        return Err(invalid());
    }
    let origin = url::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(origin.origin().ascii_serialization())
}

fn absolute_or_resolve(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_build_version_without_an_agent_command() {
        let result = Options::try_parse_from(["attyd", "--version"]).unwrap_err();
        assert_eq!(result.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(result.to_string(), format!("attyd {}\n", crate::VERSION));
        assert!(!result.use_stderr());
    }

    #[test]
    fn accepts_all_signed_unobserved_timeout_policies() {
        for seconds in [
            "-9223372036854775808",
            "-42",
            "-1",
            "0",
            "1800",
            "9223372036854775807",
        ] {
            let options = Options::try_parse_from([
                "attyd",
                "--session-unobserved-timeout",
                seconds,
                "--",
                "agent",
            ])
            .unwrap();
            assert_eq!(
                options.session_unobserved_timeout,
                seconds.parse::<i64>().unwrap()
            );
            assert_eq!(options.command, ["agent"]);
        }
    }

    #[test]
    fn keeps_the_existing_cli_surface() {
        let options = Options::try_parse_from([
            "attyd",
            "-H",
            "0.0.0.0",
            "-p",
            "7444",
            "-t",
            "ws",
            "--",
            "ws://127.0.0.1:3284/acp",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        assert_eq!(options.host.to_string(), "0.0.0.0");
        assert_eq!(options.port, 7444);
        assert_eq!(options.transport, Transport::Ws);
        assert_eq!(options.command, ["ws://127.0.0.1:3284/acp"]);
    }

    #[test]
    fn rejects_remote_extra_arguments() {
        let options =
            Options::try_parse_from(["attyd", "-t", "http", "--", "http://127.0.0.1/acp", "extra"])
                .unwrap();
        assert!(options.normalized().unwrap_err().contains("exactly one"));
    }

    #[test]
    fn accepts_an_ephemeral_port_for_integration_tests() {
        let options = Options::try_parse_from(["attyd", "--port", "0", "--", "fake-agent"])
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(options.port, 0);
    }

    #[test]
    fn defaults_to_stdio_and_accepts_remote_transport_aliases() {
        let local = Options::try_parse_from(["attyd", "--", "example-agent", "acp"])
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(local.transport, Transport::Stdio);
        assert_eq!(local.command.last().map(String::as_str), Some("acp"));
        assert_eq!(local.command[0], "example-agent");

        for (transport, endpoint, expected) in [
            ("http", "https://agent.example/acp", Transport::Http),
            ("sse", "http://127.0.0.1:3284/acp", Transport::Http),
            (
                "streamable-http",
                "https://agent.example/acp",
                Transport::Http,
            ),
            ("ws", "wss://agent.example/acp", Transport::Ws),
        ] {
            let options = Options::try_parse_from(["attyd", "-t", transport, endpoint])
                .unwrap()
                .normalized()
                .unwrap();
            assert_eq!(options.transport, expected);
            assert_eq!(options.command, [endpoint]);
        }
    }

    #[test]
    fn requires_an_explicit_stdio_command() {
        let error = Options::try_parse_from(["attyd"])
            .unwrap()
            .normalized()
            .unwrap_err();
        assert!(error.contains("-- <agent-command> [args...]"));
    }

    #[test]
    fn accepts_only_explicit_http_origins_and_normalizes_default_ports() {
        for (origin, expected) in [
            ("https://agent.example:443", "https://agent.example"),
            ("http://localhost:7331", "http://localhost:7331"),
            ("http://[::1]:7331", "http://[::1]:7331"),
        ] {
            assert_eq!(normalize_origin(origin).unwrap(), expected);
        }
        for origin in [
            "null",
            "*",
            "ftp://example.com",
            "https://*.example.com",
            "https://user:pass@example.com",
            "https://example.com/",
            "https://example.com/path",
            "https://example.com?x=1",
            "https://example.com#fragment",
            " https://example.com",
            "https://example.com\\other",
        ] {
            assert!(normalize_origin(origin).is_err(), "accepted {origin}");
        }
    }

    #[test]
    fn validates_remote_protocols_and_local_only_roots() {
        for arguments in [
            vec!["attyd", "-t", "http"],
            vec!["attyd", "-t", "ws", "https://agent.example/acp"],
            vec!["attyd", "-t", "http", "ws://agent.example/acp"],
            vec![
                "attyd",
                "-t",
                "ws",
                "--add-dir",
                "/tmp/shared",
                "ws://agent.example/acp",
            ],
        ] {
            let parsed = Options::try_parse_from(arguments).unwrap();
            assert!(parsed.normalized().is_err());
        }
        assert!(Options::try_parse_from(["attyd", "-t", "pipe"]).is_err());
    }

    #[test]
    fn deduplicates_local_roots_without_treating_them_as_agent_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_string_lossy().into_owned();
        let options = Options::try_parse_from([
            "attyd",
            "--add-dir",
            root.as_str(),
            "--add-dir",
            root.as_str(),
            "--",
            "agent",
            "acp",
        ])
        .unwrap()
        .normalized()
        .unwrap();
        assert_eq!(options.additional_directories, [directory.path()]);
        assert_eq!(options.command, ["agent", "acp"]);
    }
}
