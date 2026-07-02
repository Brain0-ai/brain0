//! Generic MCP transport (JSON-RPC 2.0 over stdio) for brain0.
//!
//! **PRD 2:** the old MCP *ingest* role (receiving agent-pushed prompts/decisions/declared
//! changes) has been removed — brain0 is a pure observer and never relies on agent
//! cooperation. This crate now provides only the reusable JSON-RPC transport; the brain0
//! MCP **query** channel supplies its tool set via a [`ToolProvider`] (see
//! `crates/brain0-cli`,).

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The MCP protocol version brain0 speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A tool advertised over MCP.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The result of a tool call: human/agent-facing text and an error flag.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Supplies the tool set and handles tool calls. brain0's query channel implements this.
pub trait ToolProvider {
    fn tools(&self) -> Vec<ToolDef>;
    fn call(&self, name: &str, arguments: &Value) -> ToolOutcome;
}

/// A minimal, dependency-free JSON-RPC 2.0 server over a [`ToolProvider`], so the observer
/// stays a single self-contained binary.
pub struct JsonRpcServer<P> {
    provider: P,
}

impl<P> std::fmt::Debug for JsonRpcServer<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcServer").finish_non_exhaustive()
    }
}

impl<P: ToolProvider> JsonRpcServer<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Handle one parsed JSON-RPC message. Returns the response, or `None` for
    /// notifications (messages without an `id`).
    pub fn handle_message(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => Some(ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "brain0-mcp", "version": env!("CARGO_PKG_VERSION")}
                }),
            )),
            "tools/list" => {
                let tools: Vec<Value> = self
                    .provider
                    .tools()
                    .into_iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                        })
                    })
                    .collect();
                Some(ok(id, json!({ "tools": tools })))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                let outcome = self.provider.call(name, &args);
                Some(ok(
                    id,
                    json!({
                        "content": [{"type": "text", "text": outcome.text}],
                        "isError": outcome.is_error
                    }),
                ))
            }
            "ping" => Some(ok(id, json!({}))),
            other => Some(err(id, -32601, &format!("method not found: {other}"))),
        }
    }

    /// Serve over a reader/writer using newline-delimited JSON-RPC (MCP stdio transport).
    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue; // ignore malformed frames
            };
            if let Some(response) = self.handle_message(&message) {
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
            }
        }
        Ok(())
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProvider;
    impl ToolProvider for StubProvider {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef {
                name: "echo".into(),
                description: "echo back".into(),
                input_schema: json!({"type": "object"}),
            }]
        }
        fn call(&self, name: &str, arguments: &Value) -> ToolOutcome {
            if name == "echo" {
                ToolOutcome::ok(arguments.to_string())
            } else {
                ToolOutcome::error(format!("unknown tool: {name}"))
            }
        }
    }

    fn server() -> JsonRpcServer<StubProvider> {
        JsonRpcServer::new(StubProvider)
    }

    #[test]
    fn initialize_and_tools_list() {
        let s = server();
        let init = s
            .handle_message(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        let list = s
            .handle_message(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
    }

    #[test]
    fn notifications_get_no_response() {
        assert!(server()
            .handle_message(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
    }

    #[test]
    fn tool_call_dispatches_to_provider() {
        let resp = server()
            .handle_message(&json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"echo","arguments":{"x":1}}
            }))
            .unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let unknown = server()
            .handle_message(&json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"nope","arguments":{}}
            }))
            .unwrap();
        assert_eq!(unknown["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_an_error() {
        let resp = server()
            .handle_message(&json!({"jsonrpc":"2.0","id":9,"method":"frobnicate"}))
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
