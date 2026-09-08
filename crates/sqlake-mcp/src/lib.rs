//! The agent surface as MCP tools, over stdio.
//!
//! A wrapper, not a second surface. Every tool is one [`Request`] and its
//! answer is that request's [`Response`], so anything an MCP client can do a
//! `sqlake` subcommand can do too — and the guards are the same ones, because
//! it is the same `Service` underneath.
//!
//! Its own crate so the MCP SDK stays out of the TUI's dependency tree, which
//! is the rule design-agent.md §6 states and this is the crate that would have
//! broken it.
//!
//! The tool list is generated from the protocol types rather than written
//! here. A hand-written one is a second description of the surface that
//! nothing checks, and it drifts in the direction that matters: it goes on
//! describing a call after the call has changed.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServiceExt as _};
use serde_json::Value as Json;
use sqlake_api::{Backend, Failure, Request, RequestKind, Response, choose};
use tokio::sync::Mutex;

pub mod tools;

pub use tools::{all as tools_list, kind_of};

/// Serves the agent surface as MCP tools.
#[derive(Debug)]
pub struct Server {
    /// One at a time. The socket is a single connection and a local store is a
    /// single actor, so concurrent tool calls would queue inside either one
    /// anyway — here the wait is visible instead of buried.
    backend: Mutex<Backend>,
    /// Which profile to prefer when a tool names no connection. What
    /// `--connect` sets for a subcommand.
    connect: Option<String>,
}

impl Server {
    #[must_use]
    pub fn new(backend: Backend, connect: Option<String>) -> Self {
        Self {
            backend: Mutex::new(backend),
            connect,
        }
    }

    /// Serve on stdin and stdout until the client disconnects.
    ///
    /// # Errors
    ///
    /// A transport that could not be established, or that failed while running.
    pub async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let running = self.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    }

    /// Turn a tool call into the request it names, and answer it.
    pub async fn answer(&self, call: CallToolRequestParams) -> Response {
        match self.dispatch(call).await {
            Ok(response) => response,
            Err(failure) => Response::Failed(failure),
        }
    }

    async fn dispatch(&self, call: CallToolRequestParams) -> Result<Response, Failure> {
        let kind = tools::kind_of(&call.name).ok_or_else(|| Failure::Malformed {
            message: format!("no tool called `{}`", call.name),
        })?;

        let mut fields = call.arguments.unwrap_or_default();
        // The tool's name *is* the request's tag, so a caller never writes it
        // — which also means it cannot write the wrong one.
        fields.insert("request".to_owned(), Json::from(kind.tag()));
        if tools::takes_a_connection(kind) && !fields.contains_key("connection") {
            let picked = self.pick(kind).await?;
            fields.insert("connection".to_owned(), Json::from(picked));
        }

        // Through the protocol's own deserialiser, so `deny_unknown_fields`
        // and every type in the request apply to a tool call exactly as they
        // apply to a line on the socket.
        let request: Request =
            serde_json::from_value(Json::Object(fields)).map_err(|why| Failure::Malformed {
                message: format!("`{}` was called with {why}", call.name),
            })?;
        self.send(&request).await
    }

    async fn send(&self, request: &Request) -> Result<Response, Failure> {
        self.backend
            .lock()
            .await
            .request(request)
            .await
            .map_err(|why| Failure::Driver {
                message: format!("the session stopped answering: {why}"),
            })
    }

    /// The connection a tool that named none acts on.
    ///
    /// The same rules a subcommand follows, from the same function: a caller
    /// that had to list connections before every call would spend half its
    /// turns on bookkeeping, and one that guessed would guess differently from
    /// the CLI.
    async fn pick(&self, kind: RequestKind) -> Result<String, Failure> {
        let listed = self.send(&Request::ConnectionList {}).await?;
        let Response::Connections(open) = listed else {
            return Err(Failure::Malformed {
                message: format!("the session answered a connection list with {listed:?}"),
            });
        };
        choose(&open, self.connect.as_deref(), tools::is_destructive(kind))
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        let mut server_info = Implementation::default();
        server_info.name = "sqlake".to_owned();
        server_info.version = env!("CARGO_PKG_VERSION").to_owned();
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(
            "Read and query the databases this sqlake session has open. Every tool that \
                 acts on a connection takes an optional `connection` id from `connection_list`; \
                 without one, the session's ready connection is used. `query_run` answers a \
                 handle — `query_wait` blocks for the rows. A query over the byte budget comes \
                 back `needs_approval` with what it would cost: that number is for a person, \
                 and nothing here can approve it."
                .to_owned(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: tools::all(),
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let response = self.answer(request).await;
        let json = serde_json::to_string_pretty(&response).map_err(|why| {
            McpError::internal_error(format!("the answer would not serialise: {why}"), None)
        })?;
        let content = vec![ContentBlock::text(json)];
        // A failure the surface produced is a *tool* error, not a protocol
        // one. The request was understood and routed; what failed is the
        // database, and a client handed a JSON-RPC error renders it opaquely
        // and shows the caller nothing. "This table is not there" is something
        // to read.
        Ok(if matches!(response, Response::Failed(_)) {
            CallToolResult::error(content).into()
        } else {
            CallToolResult::success(content).into()
        })
    }
}

/// One tool, as `list_tools` describes it.
#[must_use]
pub fn tool(kind: RequestKind) -> Tool {
    let mut tool = Tool::new(
        Cow::Borrowed(kind.tag()),
        Cow::Borrowed(tools::describes(kind)),
        Arc::new(tools::input_schema(kind)),
    );
    // Hints, and the ones a client actually acts on: an agent asked to be
    // careful is told which of these change something before it calls one.
    let mut hints = ToolAnnotations::default();
    hints.read_only_hint = Some(!tools::writes(kind));
    hints.destructive_hint = Some(tools::is_destructive(kind));
    tool.annotations = Some(hints);
    tool
}
