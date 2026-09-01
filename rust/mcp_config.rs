use std::collections::HashSet;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    EnvVariable, HttpHeader, McpServer, McpServerAcp, McpServerHttp, McpServerSse, McpServerStdio,
};
use serde_json::Value;

const MAX_MCP_SERVERS: usize = 32;

#[derive(Clone, Debug)]
pub struct AcpMcpProvider {
    pub name: String,
    pub server_id: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvVariable>,
}

pub fn load_mcp_configs(
    paths: &[PathBuf],
) -> Result<(Vec<McpServer>, Vec<AcpMcpProvider>), String> {
    let mut servers = Vec::new();
    let mut providers = Vec::new();
    for path in paths {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read MCP config {}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&source)
            .map_err(|error| format!("MCP config {} is not valid JSON: {error}", path.display()))?;
        let entries = value
            .as_array()
            .or_else(|| value.get("mcpServers").and_then(Value::as_array))
            .ok_or_else(|| {
                format!(
                    "MCP config {} must be an array or contain mcpServers",
                    path.display()
                )
            })?;
        for (index, entry) in entries.iter().enumerate() {
            let label = format!("{}[{index}]", path.display());
            let (server, provider) = parse_server(entry, &label)?;
            servers.push(server);
            providers.extend(provider);
            if servers.len() > MAX_MCP_SERVERS {
                return Err(format!(
                    "MCP configs contain more than {MAX_MCP_SERVERS} servers"
                ));
            }
        }
    }

    let mut names = HashSet::new();
    for server in &servers {
        let name = server_name(server);
        if !names.insert(name.to_string()) {
            return Err(format!("MCP configs contain duplicate server name: {name}"));
        }
    }
    let mut ids = HashSet::new();
    for provider in &providers {
        if !ids.insert(provider.server_id.clone()) {
            return Err(format!(
                "MCP configs contain duplicate ACP MCP serverId: {}",
                provider.server_id
            ));
        }
    }
    Ok((servers, providers))
}

pub fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Http(server) => &server.name,
        McpServer::Sse(server) => &server.name,
        McpServer::Acp(server) => &server.name,
        McpServer::Stdio(server) => &server.name,
        _ => "unknown",
    }
}

pub fn server_type(server: &McpServer) -> &'static str {
    match server {
        McpServer::Http(_) => "http",
        McpServer::Sse(_) => "sse",
        McpServer::Acp(_) => "acp",
        McpServer::Stdio(_) => "stdio",
        _ => "unknown",
    }
}

fn parse_server(value: &Value, label: &str) -> Result<(McpServer, Option<AcpMcpProvider>), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let name = required_string(value, "name", label)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("stdio");
    match kind {
        "stdio" => {
            let command = absolute_command(required_string(value, "command", label)?, label)?;
            let args = string_array(object.get("args"), &format!("{label}.args"))?;
            let env = env_array(object.get("env"), &format!("{label}.env"))?;
            Ok((
                McpServer::Stdio(McpServerStdio::new(name, command).args(args).env(env)),
                None,
            ))
        }
        "http" | "sse" => {
            let raw_url = required_string(value, "url", label)?;
            let url = url::Url::parse(&raw_url)
                .map_err(|_| format!("{label}.url must be an absolute HTTP(S) URL"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("{label}.url must be an absolute HTTP(S) URL"));
            }
            let headers = header_array(object.get("headers"), &format!("{label}.headers"))?;
            if kind == "http" {
                Ok((
                    McpServer::Http(McpServerHttp::new(name, url.to_string()).headers(headers)),
                    None,
                ))
            } else {
                Ok((
                    McpServer::Sse(McpServerSse::new(name, url.to_string()).headers(headers)),
                    None,
                ))
            }
        }
        "acp" => {
            let server_id = required_string(value, "serverId", label)?;
            let command = absolute_command(required_string(value, "command", label)?, label)?;
            let args = string_array(object.get("args"), &format!("{label}.args"))?;
            let env = env_array(object.get("env"), &format!("{label}.env"))?;
            let provider = AcpMcpProvider {
                name: name.clone(),
                server_id: server_id.clone(),
                command,
                args,
                env,
            };
            Ok((
                McpServer::Acp(McpServerAcp::new(name, server_id)),
                Some(provider),
            ))
        }
        other => Err(format!("{label}.type is unsupported: {other}")),
    }
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{label}.{field} must be a non-empty string"))
}

fn absolute_command(command: String, label: &str) -> Result<PathBuf, String> {
    let command = PathBuf::from(command);
    if !command.is_absolute() {
        return Err(format!("{label}.command must be an absolute path"));
    }
    Ok(command)
}

fn string_array(value: Option<&Value>, label: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .filter(|values| values.iter().all(Value::is_string))
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .ok_or_else(|| format!("{label} must be an array of strings"))
}

fn env_array(value: Option<&Value>, label: &str) -> Result<Vec<EnvVariable>, String> {
    key_value_array(value, label).map(|values| {
        values
            .into_iter()
            .map(|(name, value)| EnvVariable::new(name, value))
            .collect()
    })
}

fn header_array(value: Option<&Value>, label: &str) -> Result<Vec<HttpHeader>, String> {
    key_value_array(value, label).map(|values| {
        values
            .into_iter()
            .map(|(name, value)| HttpHeader::new(name, value))
            .collect()
    })
}

fn key_value_array(value: Option<&Value>, label: &str) -> Result<Vec<(String, String)>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{label} must be an array"))?;
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            Ok((
                required_string(value, "name", &format!("{label}[{index}]"))?,
                required_string(value, "value", &format!("{label}[{index}]"))?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(directory: &tempfile::TempDir, name: &str, value: Value) -> PathBuf {
        let path = directory.path().join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path
    }

    #[test]
    fn parses_all_transport_shapes_without_exposing_acp_launch_details() {
        let directory = tempfile::tempdir().unwrap();
        let command = std::env::current_exe().unwrap();
        let source = serde_json::json!({
            "mcpServers": [
                { "name": "local", "command": command, "args": ["--stdio"] },
                { "type": "http", "name": "remote", "url": "https://example.com/mcp" },
                { "type": "acp", "name": "private", "serverId": "private-id", "command": command }
            ]
        });
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, source.to_string()).unwrap();
        let (servers, providers) = load_mcp_configs(&[path]).unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].server_id, "private-id");
    }

    #[test]
    fn rejects_unsafe_ambiguous_and_duplicate_definitions() {
        let directory = tempfile::tempdir().unwrap();
        for (name, source) in [
            (
                "relative.json",
                serde_json::json!([{ "name": "x", "command": "relative" }]),
            ),
            (
                "file-url.json",
                serde_json::json!([{ "name": "x", "type": "http", "url": "file:///tmp/x" }]),
            ),
            (
                "missing-command.json",
                serde_json::json!([{ "name": "x", "type": "acp", "serverId": "x" }]),
            ),
        ] {
            let path = write_config(&directory, name, source);
            assert!(load_mcp_configs(&[path]).is_err());
        }

        let command = std::env::current_exe().unwrap();
        let duplicate_names = write_config(
            &directory,
            "duplicate-names.json",
            serde_json::json!([
                { "name": "same", "command": command },
                { "name": "same", "command": command }
            ]),
        );
        assert!(
            load_mcp_configs(&[duplicate_names])
                .unwrap_err()
                .contains("duplicate server name")
        );
        let duplicate_ids = write_config(
            &directory,
            "duplicate-ids.json",
            serde_json::json!([
                { "name": "one", "type": "acp", "serverId": "same", "command": command },
                { "name": "two", "type": "acp", "serverId": "same", "command": command }
            ]),
        );
        assert!(
            load_mcp_configs(&[duplicate_ids])
                .unwrap_err()
                .contains("duplicate ACP MCP serverId")
        );
    }

    #[test]
    fn enforces_the_combined_server_limit_across_config_files() {
        let directory = tempfile::tempdir().unwrap();
        let command = std::env::current_exe().unwrap();
        let first = write_config(
            &directory,
            "first.json",
            serde_json::json!([
                { "name": "first-0", "command": command },
                { "name": "first-1", "command": command }
            ]),
        );
        let overflow = write_config(
            &directory,
            "overflow.json",
            Value::Array(
                (0..31)
                    .map(|index| {
                        serde_json::json!({
                            "name": format!("overflow-{index}"),
                            "command": command,
                        })
                    })
                    .collect(),
            ),
        );
        assert!(
            load_mcp_configs(&[first, overflow])
                .unwrap_err()
                .contains("more than 32")
        );
    }

    #[test]
    fn rejects_malformed_argument_environment_and_header_collections() {
        let directory = tempfile::tempdir().unwrap();
        let command = std::env::current_exe().unwrap();
        for (name, source) in [
            (
                "args.json",
                serde_json::json!([{ "name": "x", "command": command, "args": ["ok", 1] }]),
            ),
            (
                "env.json",
                serde_json::json!([{ "name": "x", "command": command, "env": [{ "name": "A" }] }]),
            ),
            (
                "headers.json",
                serde_json::json!([{
                    "name": "x",
                    "type": "http",
                    "url": "https://example.test/mcp",
                    "headers": { "Authorization": "secret" }
                }]),
            ),
        ] {
            let path = write_config(&directory, name, source);
            assert!(load_mcp_configs(&[path]).is_err());
        }
    }
}
