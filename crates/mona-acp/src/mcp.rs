//! Session-scoped Streamable HTTP MCP clients for ACP hosts.
//!
//! Hosts may supply HTTP endpoints in `session/new` or `session/resume`.
//! These are deliberately kept in memory: header values and MCP session IDs
//! are connection credentials, not durable Mona session metadata. Stdio and
//! legacy SSE are rejected, so an ACP host cannot turn this process into an
//! arbitrary local command launcher.

use anyhow::{Context, Result, bail, ensure};
use mona_message_types::ToolDefinition;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

/// Monitter's broker currently supports these Streamable HTTP MCP protocol
/// revisions; use the older supported revision for widest compatibility.
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
pub const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone)]
struct HttpServerSpec {
    name: String,
    url: String,
    headers: HashMap<String, String>,
}

#[derive(Clone)]
struct HttpMcpServer {
    spec: HttpServerSpec,
    client: reqwest::Client,
    next_id: Arc<AtomicU64>,
    session_id: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
struct RemoteTool {
    server: HttpMcpServer,
    remote_name: String,
    definition: ToolDefinition,
}

/// Runtime-only MCP surface for one ACP session.
#[derive(Clone, Default)]
pub struct SessionMcpTools {
    tools: Arc<HashMap<String, RemoteTool>>,
}

impl SessionMcpTools {
    /// Validate, initialize, and list tools from the ACP `mcpServers` array.
    /// An omitted field and an empty list both produce an empty remote surface.
    pub async fn from_acp_params(params: &Value) -> Result<Self> {
        let Some(value) = params.get("mcpServers") else {
            return Ok(Self::default());
        };
        if value.is_null() {
            return Ok(Self::default());
        }
        let specs = parse_http_servers(value)?;
        let mut tools = HashMap::new();
        for spec in specs {
            let server = HttpMcpServer::connect(spec).await?;
            for remote in server.list_tools().await? {
                let key = remote_tool_key(&server.spec.name, &remote.name);
                ensure!(
                    !tools.contains_key(&key),
                    "duplicate HTTP MCP tool name '{key}'"
                );
                tools.insert(
                    key.clone(),
                    RemoteTool {
                        server: server.clone(),
                        remote_name: remote.name,
                        definition: ToolDefinition {
                            name: key,
                            description: remote
                                .description
                                .unwrap_or_else(|| format!("MCP tool from {}", server.spec.name)),
                            input_schema: remote.input_schema,
                        },
                    },
                );
            }
        }
        Ok(Self {
            tools: Arc::new(tools),
        })
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self
            .tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn call(&self, name: &str, input: Value) -> Result<Option<Value>> {
        let Some(tool) = self.tools.get(name) else {
            return Ok(None);
        };
        Ok(Some(tool.server.call_tool(&tool.remote_name, input).await?))
    }
}

#[derive(Debug, serde::Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolDefinition>,
}

#[derive(Debug, serde::Deserialize)]
struct McpToolDefinition {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    input_schema: Value,
}

fn remote_tool_key(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

fn parse_http_servers(value: &Value) -> Result<Vec<HttpServerSpec>> {
    let servers = value
        .as_array()
        .context("ACP mcpServers must be an array")?;
    let mut names = HashSet::new();
    let mut result = Vec::with_capacity(servers.len());
    for value in servers {
        let object = value
            .as_object()
            .context("ACP mcpServers entries must be objects")?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .context("ACP MCP server requires a non-empty name")?;
        ensure!(
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "ACP MCP server name '{name}' contains unsupported characters"
        );
        ensure!(
            names.insert(name.to_string()),
            "duplicate ACP MCP server '{name}'"
        );
        let transport = object.get("type").and_then(Value::as_str).unwrap_or("http");
        ensure!(
            matches!(
                transport.to_ascii_lowercase().as_str(),
                "http" | "streamable-http"
            ),
            "ACP MCP server '{name}' must use HTTP, not '{transport}'"
        );
        let url = object
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .with_context(|| format!("ACP HTTP MCP server '{name}' requires a url"))?;
        let parsed = url::Url::parse(url)
            .with_context(|| format!("ACP HTTP MCP server '{name}' has an invalid url"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "ACP HTTP MCP server '{name}' url must use http or https"
        );
        let mut headers = HashMap::new();
        if let Some(value) = object.get("headers") {
            let pairs: Vec<(String, String)> = if let Some(object) = value.as_object() {
                object
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            key.clone(),
                            value
                                .as_str()
                                .with_context(|| format!("ACP HTTP MCP server '{name}' header '{key}' must be a string"))?
                                .to_string(),
                        ))
                    })
                    .collect::<Result<_>>()?
            } else if let Some(values) = value.as_array() {
                values
                    .iter()
                    .map(|value| {
                        let header = value.as_object().with_context(|| format!("ACP HTTP MCP server '{name}' header entries must be objects"))?;
                        let key = header.get("name").and_then(Value::as_str).map(str::trim)
                            .filter(|value| !value.is_empty())
                            .with_context(|| format!("ACP HTTP MCP server '{name}' header requires a name"))?;
                        let value = header.get("value").and_then(Value::as_str)
                            .with_context(|| format!("ACP HTTP MCP server '{name}' header '{key}' requires a string value"))?;
                        Ok((key.to_string(), value.to_string()))
                    })
                    .collect::<Result<_>>()?
            } else {
                bail!("ACP HTTP MCP server '{name}' headers must be an array or object")
            };
            for (key, value) in pairs {
                ensure!(
                    headers.insert(key.to_ascii_lowercase(), value).is_none(),
                    "ACP HTTP MCP server '{name}' repeats header '{key}'"
                );
            }
        }
        result.push(HttpServerSpec {
            name: name.to_string(),
            url: url.to_string(),
            headers,
        });
    }
    Ok(result)
}

impl HttpMcpServer {
    async fn connect(spec: HttpServerSpec) -> Result<Self> {
        let server = Self {
            spec,
            client: reqwest::Client::builder()
                .timeout(MCP_CONNECT_TIMEOUT)
                .build()?,
            next_id: Arc::new(AtomicU64::new(1)),
            session_id: Arc::new(Mutex::new(None)),
        };
        let initialize = server
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "mona-acp", "version": env!("CARGO_PKG_VERSION") }
                })),
            )
            .await?;
        ensure!(
            initialize.get("result").is_some(),
            "HTTP MCP server '{}' did not return initialize result",
            server.spec.name
        );
        server.notify("notifications/initialized", None).await?;
        Ok(server)
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>> {
        let response = self.request("tools/list", None).await?;
        let result = response
            .get("result")
            .cloned()
            .context("HTTP MCP tools/list missing result")?;
        Ok(serde_json::from_value::<ToolsListResult>(result)?.tools)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let response = self
            .request(
                "tools/call",
                Some(json!({ "name": name, "arguments": arguments })),
            )
            .await?;
        response
            .get("result")
            .cloned()
            .context("HTTP MCP tools/call missing result")
    }

    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        self.ensure_jsonrpc_success(&response)?;
        Ok(response)
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let response = self
            .post(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await?;
        self.ensure_jsonrpc_success(&response)?;
        Ok(())
    }

    fn ensure_jsonrpc_success(&self, response: &Value) -> Result<()> {
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown MCP error");
            bail!(
                "HTTP MCP server '{}' returned JSON-RPC error {code}: {message}",
                self.spec.name
            );
        }
        Ok(())
    }

    async fn post(&self, payload: Value) -> Result<Value> {
        let mut request = self
            .client
            .post(&self.spec.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&payload);
        for (name, value) in &self.spec.headers {
            request = request.header(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid MCP header name '{name}'"))?,
                reqwest::header::HeaderValue::from_str(value)
                    .with_context(|| format!("invalid MCP header value for '{name}'"))?,
            );
        }
        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header("mcp-session-id", session_id);
        }
        let response = request
            .send()
            .await
            .context("HTTP MCP request failed")?
            .error_for_status()?;
        if let Some(session_id) = response.headers().get("mcp-session-id") {
            *self.session_id.lock().await = Some(
                session_id
                    .to_str()
                    .context("invalid HTTP MCP session id")?
                    .to_string(),
            );
        }
        let body = response.text().await.context("read HTTP MCP response")?;
        if body.trim().is_empty() {
            return Ok(json!({}));
        }
        if let Ok(value) = serde_json::from_str(&body) {
            return Ok(value);
        }
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            bail!("HTTP MCP response was neither JSON nor SSE data")
        }
        serde_json::from_str(&data).context("invalid JSON-RPC payload in HTTP MCP SSE response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn rejects_stdio_and_normalizes_remote_names() {
        assert!(parse_http_servers(&json!([{"name":"bad","type":"stdio","command":"x"}])).is_err());
        assert_eq!(remote_tool_key("search", "find"), "mcp__search__find");
    }

    #[test]
    fn rejects_bad_url_and_header_shapes() {
        assert!(parse_http_servers(&json!([{"name":"bad","url":"file:///tmp/mcp"}])).is_err());
        assert!(
            parse_http_servers(
                &json!([{"name":"bad","url":"https://example.test/mcp","headers":{"x":1}}])
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn monitter_header_array_initializes_lists_and_calls_over_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let captured = seen.clone();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                let mut byte = [0u8; 1];
                while !header.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                }
                let header_text = String::from_utf8(header).unwrap();
                let content_length = header_text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                let mut body = vec![0; content_length];
                stream.read_exact(&mut body).await.unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let method = request["method"].as_str().unwrap_or_default().to_string();
                captured
                    .lock()
                    .await
                    .push((method.clone(), header_text.clone()));
                let (status, content_type, response) = match method.as_str() {
                    "initialize" => (
                        "200 OK",
                        "application/json",
                        json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}).to_string(),
                    ),
                    "notifications/initialized" => ("202 Accepted", "application/json", String::new()),
                    "tools/list" => (
                        "200 OK",
                        "text/event-stream",
                        format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"lookup","description":"Look up a record","inputSchema":{"type":"object"}}]}})),
                    ),
                    "tools/call" => (
                        "200 OK",
                        "application/json",
                        json!({"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"ok"}]}}).to_string(),
                    ),
                    other => panic!("unexpected MCP method {other}"),
                };
                let session_header = if method == "initialize" {
                    "mcp-session-id: test-session\r\n"
                } else {
                    ""
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{session_header}content-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });

        let tools = SessionMcpTools::from_acp_params(&json!({
            "mcpServers": [{
                "name": "broker",
                "type": "http",
                "url": format!("http://{address}/mcp"),
                "headers": [{"name": "Authorization", "value": "Bearer fixture-token"}]
            }]
        }))
        .await
        .unwrap();
        assert_eq!(tools.definitions()[0].name, "mcp__broker__lookup");
        assert_eq!(
            tools
                .call("mcp__broker__lookup", json!({"q":"x"}))
                .await
                .unwrap(),
            Some(json!({"content":[{"type":"text","text":"ok"}]}))
        );
        server.await.unwrap();
        let seen = seen.lock().await;
        assert_eq!(
            seen.iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        for (_, headers) in seen.iter() {
            assert!(headers.contains("accept: application/json, text/event-stream"));
            assert!(headers.contains("mcp-protocol-version: 2025-03-26"));
            assert!(headers.contains("authorization: Bearer fixture-token"));
        }
        assert!(seen[2].1.contains("mcp-session-id: test-session"));
        assert!(seen[3].1.contains("mcp-session-id: test-session"));
    }
}
