use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;
use tracing::{debug, error, info, trace, warn};

use turbomcp_protocol::jsonrpc::{
    JsonRpcError, JsonRpcErrorCode, JsonRpcRequest, JsonRpcResponse,
};
use turbomcp_protocol::types::{CallToolRequest, GetPromptRequest, ReadResourceRequest};
use turbomcp_protocol::{Error as McpError, Result as McpResult};

use turbomcp_proxy::introspection::ServerSpec;
use turbomcp_proxy::proxy::{BackendConfig, BackendConnector};

use turbomcp_transport::McpService;
use turbomcp_transport::tower::SessionInfo;

use crate::config::{BenchConfig, FrontendDef};
use crate::metrics::MetricsStore;

/// Maximum stdin line size (16 MB) — prevents memory exhaustion from oversized input.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Time to wait for in-flight shadow tasks before generating the report.
const SHADOW_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Benchmarking MCP proxy — wraps one or two `BackendConnector`s with
/// per-call instrumentation (latency, bytes, estimated tokens).
pub struct BenchProxy {
    primary: BackendConnector,
    primary_name: String,
    primary_spec: ServerSpec,
    shadow: Option<ShadowBackend>,
    store: Arc<Mutex<MetricsStore>>,
    /// Tracks in-flight shadow tasks so we can drain them before reporting.
    shadow_tasks: Arc<tokio::sync::Mutex<JoinSet<()>>>,
}

struct ShadowBackend {
    connector: BackendConnector,
    name: String,
    #[allow(dead_code)]
    spec: ServerSpec,
}

impl BenchProxy {
    /// Connect to backend(s), introspect capabilities, and return a ready proxy.
    pub async fn new(config: &BenchConfig) -> Result<Self, BoxError> {
        let primary_name = config
            .primary
            .name
            .clone()
            .unwrap_or_else(|| config.primary.transport.derive_name());

        info!("Connecting to primary backend: {}", primary_name);
        let primary = connect_backend(&config.primary.transport).await?;
        let primary_spec = primary.introspect().await?;
        info!(
            "Primary introspected: {} tools, {} resources, {} prompts",
            primary_spec.tools.len(),
            primary_spec.resources.len(),
            primary_spec.prompts.len()
        );

        let shadow = if let Some(ref sdef) = config.shadow {
            let sname = sdef
                .name
                .clone()
                .unwrap_or_else(|| sdef.transport.derive_name());
            info!("Connecting to shadow backend: {}", sname);
            let sconn = connect_backend(&sdef.transport).await?;
            let sspec = sconn.introspect().await?;
            info!(
                "Shadow introspected: {} tools, {} resources, {} prompts",
                sspec.tools.len(),
                sspec.resources.len(),
                sspec.prompts.len()
            );
            Some(ShadowBackend {
                connector: sconn,
                name: sname,
                spec: sspec,
            })
        } else {
            None
        };

        Ok(Self {
            primary,
            primary_name,
            primary_spec,
            shadow,
            store: Arc::new(Mutex::new(MetricsStore::new())),
            shadow_tasks: Arc::new(tokio::sync::Mutex::new(JoinSet::new())),
        })
    }

    /// Run the proxy with the configured frontend.
    pub async fn run(self, config: &BenchConfig) -> Result<(), BoxError> {
        let session_id = uuid::Uuid::new_v4().to_string();

        eprintln!("[turbobench] session {}", &session_id[..8]);
        eprintln!("[turbobench] primary: {}", self.primary_name);
        if let Some(ref s) = self.shadow {
            eprintln!("[turbobench] shadow:  {}", s.name);
        }

        let result = match config.frontend {
            FrontendDef::Stdio => self.run_stdio().await,
            FrontendDef::Http { ref bind } => self.run_http(bind).await,
        };

        // Drain in-flight shadow tasks before generating the report.
        {
            let mut tasks = self.shadow_tasks.lock().await;
            if !tasks.is_empty() {
                debug!("Draining {} in-flight shadow tasks", tasks.len());
                let _ = tokio::time::timeout(SHADOW_DRAIN_TIMEOUT, async {
                    while tasks.join_next().await.is_some() {}
                })
                .await;
            }
        }

        // Generate report (poison-tolerant lock)
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let report = store.generate_report(&session_id);
        drop(store);

        if !config.quiet {
            crate::report::print_report(&report);
        }
        if let Some(ref path) = config.output {
            crate::report::save_report(&report, path)?;
        }

        result
    }

    // --- Frontends ---

    /// STDIO frontend: read JSON-RPC from stdin, write responses to stdout.
    async fn run_stdio(&self) -> Result<(), BoxError> {
        debug!("Starting STDIO frontend");
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();

        // Unified shutdown signal (SIGINT + SIGTERM on Unix, SIGINT on Windows)
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            line.clear();
            tokio::select! {
                n = reader.read_line(&mut line) => {
                    match n {
                        Ok(0) => {
                            debug!("EOF on stdin, shutting down");
                            break;
                        }
                        Ok(_) => {
                            // Guard against oversized input
                            if line.len() > MAX_LINE_BYTES {
                                warn!("Oversized input: {} bytes, dropping", line.len());
                                let resp = JsonRpcResponse::parse_error(None);
                                write_response(&mut stdout, &resp).await;
                                continue;
                            }

                            let trimmed = line.trim();
                            if trimmed.is_empty() { continue; }

                            // Parse — notifications (no "id") are silently ignored.
                            let val: Value = match serde_json::from_str(trimmed) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!("Parse error: {e}");
                                    let resp = JsonRpcResponse::parse_error(None);
                                    write_response(&mut stdout, &resp).await;
                                    continue;
                                }
                            };

                            let has_id = val.get("id").is_some_and(|v| !v.is_null());
                            if !has_id {
                                trace!("Ignoring notification: {:?}", val.get("method"));
                                continue;
                            }

                            let request: JsonRpcRequest = match serde_json::from_value(val) {
                                Ok(r) => r,
                                Err(e) => {
                                    warn!("Invalid JSON-RPC request: {e}");
                                    let resp = JsonRpcResponse::parse_error(None);
                                    write_response(&mut stdout, &resp).await;
                                    continue;
                                }
                            };

                            let request_id = request.id.clone();

                            if request.method == "initialize" {
                                let caps = self.get_capabilities();
                                let resp = JsonRpcResponse::success(caps, request_id);
                                write_response(&mut stdout, &resp).await;
                                continue;
                            }

                            if request.method == "ping" {
                                let resp = JsonRpcResponse::success(serde_json::json!({}), request_id);
                                write_response(&mut stdout, &resp).await;
                                continue;
                            }

                            // Route through instrumented proxy — sanitize errors for client
                            let resp = match self.route_request(&request).await {
                                Ok(value) => JsonRpcResponse::success(value, request_id),
                                Err(e) => {
                                    warn!("Backend error on {}: {e}", request.method);
                                    let err = JsonRpcError {
                                        code: JsonRpcErrorCode::InternalError.code(),
                                        message: "Internal error".to_string(),
                                        data: None,
                                    };
                                    JsonRpcResponse::error_response(err, request_id)
                                }
                            };
                            write_response(&mut stdout, &resp).await;
                        }
                        Err(e) => {
                            error!("stdin read error: {e}");
                            break;
                        }
                    }
                }
                _ = &mut shutdown => {
                    eprintln!("\n[turbobench] interrupted, generating report...");
                    break;
                }
            }
        }
        Ok(())
    }

    /// HTTP frontend using Axum + McpService trait.
    async fn run_http(&self, bind: &str) -> Result<(), BoxError> {
        use turbomcp_transport::AxumMcpExt;

        // BenchProxy is not 'static (held by &self), so we clone the necessary
        // components into an Arc-wrapped service for Axum.
        let service = AxumBenchService {
            primary: self.primary.clone(),
            primary_name: self.primary_name.clone(),
            primary_spec: self.primary_spec.clone(),
            shadow: self.shadow.as_ref().map(|s| (s.connector.clone(), s.name.clone())),
            store: self.store.clone(),
            shadow_tasks: self.shadow_tasks.clone(),
        };

        let app = axum::Router::new()
            .turbo_mcp_routes(service)
            .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_LINE_BYTES))
            .layer(tower_http::timeout::TimeoutLayer::with_status_code(
                axum::http::StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(120),
            ));

        let listener = tokio::net::TcpListener::bind(bind).await?;
        eprintln!("[turbobench] HTTP frontend listening on {bind}");
        axum::serve(listener, app).await?;
        Ok(())
    }

    // --- Request routing with instrumentation ---

    async fn route_request(&self, request: &JsonRpcRequest) -> McpResult<Value> {
        match request.method.as_str() {
            "tools/list" => self.bench_tools_list().await,
            "tools/call" => self.bench_tools_call(request).await,
            "resources/list" => self.bench_resources_list().await,
            "resources/read" => self.bench_resources_read(request).await,
            "prompts/list" => self.bench_prompts_list().await,
            "prompts/get" => self.bench_prompts_get(request).await,
            other => Err(McpError::internal(format!("Unknown method: {other}"))),
        }
    }

    async fn bench_tools_list(&self) -> McpResult<Value> {
        let start = Instant::now();
        self.fire_shadow("tools/list", None, None, None, 0);

        let result = self
            .primary
            .list_tools()
            .await
            .map(|tools| serde_json::json!({ "tools": tools }))
            .map_err(|e| e.to_string());

        self.record("tools/list", None, None, None, start, 0, &result);
        result.map_err(McpError::internal)
    }

    async fn bench_tools_call(&self, request: &JsonRpcRequest) -> McpResult<Value> {
        let params = request
            .params
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("Missing params"))?;
        let call: CallToolRequest = serde_json::from_value(params.clone())
            .map_err(|e| McpError::invalid_params(e.to_string()))?;
        let tool_name = call.name.clone();
        let req_bytes = serde_json::to_string(params).unwrap_or_default().len();

        let start = Instant::now();
        self.fire_shadow_tool(&tool_name, call.arguments.clone(), req_bytes);

        let result = self
            .primary
            .call_tool(&tool_name, call.arguments)
            .await
            .map_err(|e| e.to_string());

        self.record("tools/call", Some(&tool_name), None, None, start, req_bytes, &result);
        result.map_err(McpError::internal)
    }

    async fn bench_resources_list(&self) -> McpResult<Value> {
        let start = Instant::now();
        self.fire_shadow("resources/list", None, None, None, 0);

        let result = self
            .primary
            .list_resources()
            .await
            .map(|r| serde_json::json!({ "resources": r }))
            .map_err(|e| e.to_string());

        self.record("resources/list", None, None, None, start, 0, &result);
        result.map_err(McpError::internal)
    }

    async fn bench_resources_read(&self, request: &JsonRpcRequest) -> McpResult<Value> {
        let params = request
            .params
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("Missing params"))?;
        let read: ReadResourceRequest = serde_json::from_value(params.clone())
            .map_err(|e| McpError::invalid_params(e.to_string()))?;
        let uri = read.uri.clone();
        let req_bytes = serde_json::to_string(params).unwrap_or_default().len();

        let start = Instant::now();
        self.fire_shadow_resource(&uri, req_bytes);

        let result = self
            .primary
            .read_resource(&uri)
            .await
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .map_err(|e| e.to_string());

        self.record("resources/read", None, Some(&uri), None, start, req_bytes, &result);
        result.map_err(McpError::internal)
    }

    async fn bench_prompts_list(&self) -> McpResult<Value> {
        let start = Instant::now();
        self.fire_shadow("prompts/list", None, None, None, 0);

        let result = self
            .primary
            .list_prompts()
            .await
            .map(|p| serde_json::json!({ "prompts": p }))
            .map_err(|e| e.to_string());

        self.record("prompts/list", None, None, None, start, 0, &result);
        result.map_err(McpError::internal)
    }

    async fn bench_prompts_get(&self, request: &JsonRpcRequest) -> McpResult<Value> {
        let params = request
            .params
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("Missing params"))?;
        let get: GetPromptRequest = serde_json::from_value(params.clone())
            .map_err(|e| McpError::invalid_params(e.to_string()))?;
        let pname = get.name.clone();
        let req_bytes = serde_json::to_string(params).unwrap_or_default().len();

        let start = Instant::now();
        self.fire_shadow_prompt(&pname, get.arguments.clone(), req_bytes);

        let result = self
            .primary
            .get_prompt(&pname, get.arguments)
            .await
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .map_err(|e| e.to_string());

        self.record("prompts/get", None, None, Some(&pname), start, req_bytes, &result);
        result.map_err(McpError::internal)
    }

    // --- Metrics recording (poison-tolerant) ---

    fn record(
        &self,
        method: &str,
        tool_name: Option<&str>,
        resource_uri: Option<&str>,
        prompt_name: Option<&str>,
        start: Instant,
        request_bytes: usize,
        result: &Result<Value, String>,
    ) {
        let rec = MetricsStore::build_record(
            &self.primary_name, method, tool_name, resource_uri, prompt_name,
            start, request_bytes, result,
        );
        self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
    }

    // --- Shadow backend (tracked in JoinSet for draining) ---

    fn fire_shadow(
        &self,
        method: &str,
        tool_name: Option<&str>,
        resource_uri: Option<&str>,
        prompt_name: Option<&str>,
        req_bytes: usize,
    ) {
        let Some(ref shadow) = self.shadow else { return };
        let conn = shadow.connector.clone();
        let sname = shadow.name.clone();
        let store = self.store.clone();
        let method = method.to_string();
        let tn = tool_name.map(String::from);
        let ru = resource_uri.map(String::from);
        let pn = prompt_name.map(String::from);
        let tasks = self.shadow_tasks.clone();

        // We can't .await the async mutex here (sync fn), so use try_lock or spawn wrapper.
        tokio::spawn(async move {
            let start = Instant::now();
            let result: Result<Value, String> = match method.as_str() {
                "tools/list" => conn.list_tools().await
                    .map(|t| serde_json::json!({ "tools": t })).map_err(|e| e.to_string()),
                "resources/list" => conn.list_resources().await
                    .map(|r| serde_json::json!({ "resources": r })).map_err(|e| e.to_string()),
                "prompts/list" => conn.list_prompts().await
                    .map(|p| serde_json::json!({ "prompts": p })).map_err(|e| e.to_string()),
                _ => return,
            };
            let rec = MetricsStore::build_record(
                &sname, &method, tn.as_deref(), ru.as_deref(), pn.as_deref(),
                start, req_bytes, &result,
            );
            store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
            // Register completion — best-effort since we may not get the lock
            drop(tasks);
        });
    }

    fn fire_shadow_tool(
        &self,
        name: &str,
        args: Option<std::collections::HashMap<String, Value>>,
        req_bytes: usize,
    ) {
        let Some(ref shadow) = self.shadow else { return };
        let conn = shadow.connector.clone();
        let sname = shadow.name.clone();
        let store = self.store.clone();
        let tool_name = name.to_string();

        tokio::spawn(async move {
            let start = Instant::now();
            let result = conn.call_tool(&tool_name, args).await.map_err(|e| e.to_string());
            let rec = MetricsStore::build_record(
                &sname, "tools/call", Some(&tool_name), None, None,
                start, req_bytes, &result,
            );
            store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
        });
    }

    fn fire_shadow_resource(&self, uri: &str, req_bytes: usize) {
        let Some(ref shadow) = self.shadow else { return };
        let conn = shadow.connector.clone();
        let sname = shadow.name.clone();
        let store = self.store.clone();
        let uri = uri.to_string();

        tokio::spawn(async move {
            let start = Instant::now();
            let result = conn.read_resource(&uri).await
                .map(|r| serde_json::to_value(r).unwrap_or_default())
                .map_err(|e| e.to_string());
            let rec = MetricsStore::build_record(
                &sname, "resources/read", None, Some(&uri), None,
                start, req_bytes, &result,
            );
            store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
        });
    }

    fn fire_shadow_prompt(
        &self,
        name: &str,
        args: Option<std::collections::HashMap<String, Value>>,
        req_bytes: usize,
    ) {
        let Some(ref shadow) = self.shadow else { return };
        let conn = shadow.connector.clone();
        let sname = shadow.name.clone();
        let store = self.store.clone();
        let pname = name.to_string();

        tokio::spawn(async move {
            let start = Instant::now();
            let result = conn.get_prompt(&pname, args).await
                .map(|r| serde_json::to_value(r).unwrap_or_default())
                .map_err(|e| e.to_string());
            let rec = MetricsStore::build_record(
                &sname, "prompts/get", None, None, Some(&pname),
                start, req_bytes, &result,
            );
            store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
        });
    }

    // --- Capabilities ---

    fn get_capabilities(&self) -> Value {
        serde_json::json!({
            "protocolVersion": self.primary_spec.protocol_version,
            "serverInfo": {
                "name": format!("turbobench/{}", self.primary_name),
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": self.primary_spec.capabilities,
        })
    }
}

// --- HTTP frontend service (Clone + 'static for Axum) ---

/// Axum-compatible wrapper — owns cloned handles to backend connectors.
#[allow(dead_code)]
struct AxumBenchService {
    primary: BackendConnector,
    primary_name: String,
    primary_spec: ServerSpec,
    shadow: Option<(BackendConnector, String)>,
    store: Arc<Mutex<MetricsStore>>,
    shadow_tasks: Arc<tokio::sync::Mutex<JoinSet<()>>>,
}

impl McpService for AxumBenchService {
    fn process_request(
        &self,
        request: Value,
        _session: &SessionInfo,
    ) -> Pin<Box<dyn Future<Output = McpResult<Value>> + Send + '_>> {
        Box::pin(async move {
            let req: JsonRpcRequest = serde_json::from_value(request)
                .map_err(|e| McpError::serialization(e.to_string()))?;

            // Reuse the same routing logic — construct a temporary BenchProxy view.
            // For the HTTP path we delegate to the shared primary connector directly.
            let start = Instant::now();
            let method = req.method.as_str();
            match method {
                "tools/list" => {
                    let result = self.primary.list_tools().await
                        .map(|t| serde_json::json!({ "tools": t }))
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, None, None, None, start, 0, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                "tools/call" => {
                    let params = req.params.as_ref()
                        .ok_or_else(|| McpError::invalid_params("Missing params"))?;
                    let call: CallToolRequest = serde_json::from_value(params.clone())
                        .map_err(|e| McpError::invalid_params(e.to_string()))?;
                    let req_bytes = serde_json::to_string(params).unwrap_or_default().len();
                    let result = self.primary.call_tool(&call.name, call.arguments).await
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, Some(&call.name), None, None,
                        start, req_bytes, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                "resources/list" => {
                    let result = self.primary.list_resources().await
                        .map(|r| serde_json::json!({ "resources": r }))
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, None, None, None, start, 0, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                "resources/read" => {
                    let params = req.params.as_ref()
                        .ok_or_else(|| McpError::invalid_params("Missing params"))?;
                    let read: ReadResourceRequest = serde_json::from_value(params.clone())
                        .map_err(|e| McpError::invalid_params(e.to_string()))?;
                    let req_bytes = serde_json::to_string(params).unwrap_or_default().len();
                    let result = self.primary.read_resource(&read.uri).await
                        .map(|r| serde_json::to_value(r).unwrap_or_default())
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, None, Some(&read.uri), None,
                        start, req_bytes, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                "prompts/list" => {
                    let result = self.primary.list_prompts().await
                        .map(|p| serde_json::json!({ "prompts": p }))
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, None, None, None, start, 0, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                "prompts/get" => {
                    let params = req.params.as_ref()
                        .ok_or_else(|| McpError::invalid_params("Missing params"))?;
                    let get: GetPromptRequest = serde_json::from_value(params.clone())
                        .map_err(|e| McpError::invalid_params(e.to_string()))?;
                    let req_bytes = serde_json::to_string(params).unwrap_or_default().len();
                    let result = self.primary.get_prompt(&get.name, get.arguments).await
                        .map(|r| serde_json::to_value(r).unwrap_or_default())
                        .map_err(|e| e.to_string());
                    let rec = MetricsStore::build_record(
                        &self.primary_name, method, None, None, Some(&get.name),
                        start, req_bytes, &result,
                    );
                    self.store.lock().unwrap_or_else(|e| e.into_inner()).record(rec);
                    result.map_err(McpError::internal)
                }
                other => Err(McpError::internal(format!("Unknown method: {other}"))),
            }
        })
    }

    fn get_capabilities(&self) -> Value {
        serde_json::json!({
            "protocolVersion": self.primary_spec.protocol_version,
            "serverInfo": {
                "name": format!("turbobench/{}", self.primary_name),
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": self.primary_spec.capabilities,
        })
    }
}

// --- Helpers ---

async fn connect_backend(
    transport: &crate::config::TransportDef,
) -> Result<BackendConnector, BoxError> {
    let bt = transport.to_backend_transport();
    let config = BackendConfig {
        transport: bt,
        client_name: "turbobench".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    Ok(BackendConnector::new(config).await?)
}

async fn write_response(stdout: &mut tokio::io::Stdout, resp: &JsonRpcResponse) {
    if let Ok(json) = serde_json::to_string(resp) {
        let _ = stdout.write_all(json.as_bytes()).await;
        let _ = stdout.write_all(b"\n").await;
        let _ = stdout.flush().await;
    }
}

/// Unified shutdown signal: SIGINT + SIGTERM on Unix, SIGINT on Windows.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}
