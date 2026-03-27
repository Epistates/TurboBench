use serde::Deserialize;
use std::path::PathBuf;
use turbomcp_proxy::proxy::BackendTransport;

/// Runtime configuration built from CLI args or config file.
pub struct BenchConfig {
    pub primary: BackendDef,
    pub shadow: Option<BackendDef>,
    pub frontend: FrontendDef,
    pub output: Option<PathBuf>,
    pub quiet: bool,
}

/// Defines a backend MCP server to connect to.
pub struct BackendDef {
    pub name: Option<String>,
    pub transport: TransportDef,
}

/// Frontend transport (how clients connect to turbobench).
pub enum FrontendDef {
    Stdio,
    Http { bind: String },
}

/// Transport definition — maps 1:1 to turbomcp's `BackendTransport`.
#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportDef {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        working_dir: Option<String>,
    },
    Http {
        url: String,
        auth_token: Option<String>,
    },
    Tcp {
        host: String,
        port: u16,
    },
    #[serde(rename = "websocket")]
    WebSocket {
        url: String,
    },
    #[cfg(unix)]
    Unix {
        path: String,
    },
}

impl TransportDef {
    /// Convert to turbomcp's `BackendTransport`.
    pub fn to_backend_transport(&self) -> BackendTransport {
        match self {
            Self::Stdio {
                command,
                args,
                working_dir,
            } => BackendTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                working_dir: working_dir.clone(),
            },
            Self::Http { url, auth_token } => BackendTransport::Http {
                url: url.clone(),
                auth_token: auth_token.clone(),
            },
            Self::Tcp { host, port } => BackendTransport::Tcp {
                host: host.clone(),
                port: *port,
            },
            Self::WebSocket { url } => BackendTransport::WebSocket { url: url.clone() },
            #[cfg(unix)]
            Self::Unix { path } => BackendTransport::Unix { path: path.clone() },
        }
    }

    /// Derive a human-readable name from the transport.
    pub fn derive_name(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                if args.is_empty() {
                    command.clone()
                } else {
                    format!("{} {}", command, args.join(" "))
                }
            }
            Self::Http { url, .. } => url.clone(),
            Self::Tcp { host, port } => format!("{host}:{port}"),
            Self::WebSocket { url } => url.clone(),
            #[cfg(unix)]
            Self::Unix { path } => path.clone(),
        }
    }
}

// --- TOML config file format ---

/// Top-level TOML config file structure.
#[derive(Deserialize)]
pub struct ConfigFile {
    /// Primary backend (required).
    pub primary: ConfigBackend,
    /// Optional shadow backend for A/B comparison.
    pub shadow: Option<ConfigBackend>,
    /// Frontend configuration (defaults to stdio).
    pub frontend: Option<ConfigFrontend>,
    /// Report and output options.
    pub options: Option<ConfigOptions>,
}

/// Backend definition in a TOML config file.
#[derive(Deserialize)]
pub struct ConfigBackend {
    /// Display name for reports.
    pub name: Option<String>,
    /// Transport configuration (flattened from the `type` tag).
    #[serde(flatten)]
    pub transport: TransportDef,
}

/// Frontend configuration in a TOML config file.
#[derive(Deserialize)]
pub struct ConfigFrontend {
    /// Transport type: `"stdio"` (default) or `"http"`.
    #[serde(rename = "type", default = "default_frontend_type")]
    pub transport_type: String,
    /// Bind address for HTTP frontend (default: `127.0.0.1:3000`).
    pub bind: Option<String>,
}

fn default_frontend_type() -> String {
    "stdio".to_string()
}

/// Report and output options in a TOML config file.
#[derive(Deserialize)]
pub struct ConfigOptions {
    /// Path to write the JSON report.
    pub output: Option<PathBuf>,
    /// Suppress terminal report on stderr.
    pub quiet: Option<bool>,
}

/// Load config from a TOML file and convert to `BenchConfig`.
pub fn load_config(path: &std::path::Path) -> Result<BenchConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let file: ConfigFile = toml::from_str(&content)?;

    let frontend = match file.frontend {
        Some(f) => match f.transport_type.as_str() {
            "http" => FrontendDef::Http {
                bind: f.bind.unwrap_or_else(|| "127.0.0.1:3000".to_string()),
            },
            _ => FrontendDef::Stdio,
        },
        None => FrontendDef::Stdio,
    };

    Ok(BenchConfig {
        primary: BackendDef {
            name: file.primary.name,
            transport: file.primary.transport,
        },
        shadow: file.shadow.map(|s| BackendDef {
            name: s.name,
            transport: s.transport,
        }),
        frontend,
        output: file.options.as_ref().and_then(|o| o.output.clone()),
        quiet: file.options.as_ref().and_then(|o| o.quiet).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_stdio_config() {
        let toml = r#"
[primary]
type = "stdio"
command = "npx"
args = ["@anthropic-ai/playwright-mcp"]
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        assert!(file.shadow.is_none());
        assert!(file.frontend.is_none());
        assert!(file.options.is_none());
        matches!(file.primary.transport, TransportDef::Stdio { .. });
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[primary]
type = "stdio"
command = "npx"
args = ["playwright-mcp"]
name = "playwright"

[shadow]
type = "http"
url = "http://localhost:3000"
name = "mcpsafari"

[frontend]
type = "http"
bind = "0.0.0.0:8080"

[options]
output = "report.json"
quiet = true
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(file.primary.name.as_deref(), Some("playwright"));
        assert!(file.shadow.is_some());
        assert_eq!(file.shadow.as_ref().unwrap().name.as_deref(), Some("mcpsafari"));
        assert_eq!(
            file.frontend.as_ref().unwrap().transport_type,
            "http"
        );
        assert!(file.options.as_ref().unwrap().quiet.unwrap());
    }

    #[test]
    fn parse_tcp_backend() {
        let toml = r#"
[primary]
type = "tcp"
host = "localhost"
port = 5000
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        matches!(file.primary.transport, TransportDef::Tcp { .. });
    }

    #[test]
    fn parse_websocket_backend() {
        let toml = r#"
[primary]
type = "websocket"
url = "ws://localhost:8080"
"#;
        let file: ConfigFile = toml::from_str(toml).unwrap();
        matches!(file.primary.transport, TransportDef::WebSocket { .. });
    }

    #[test]
    fn derive_name_stdio() {
        let t = TransportDef::Stdio {
            command: "npx".into(),
            args: vec!["playwright-mcp".into()],
            working_dir: None,
        };
        assert_eq!(t.derive_name(), "npx playwright-mcp");
    }

    #[test]
    fn derive_name_http() {
        let t = TransportDef::Http {
            url: "http://localhost:3000".into(),
            auth_token: None,
        };
        assert_eq!(t.derive_name(), "http://localhost:3000");
    }

    #[test]
    fn to_backend_transport_roundtrip() {
        let t = TransportDef::Tcp {
            host: "10.0.0.1".into(),
            port: 9000,
        };
        let bt = t.to_backend_transport();
        matches!(bt, BackendTransport::Tcp { .. });
    }

    #[test]
    fn missing_primary_fails() {
        let toml = r#"
[shadow]
type = "stdio"
command = "npx"
"#;
        let result: Result<ConfigFile, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_transport_type_fails() {
        let toml = r#"
[primary]
type = "grpc"
endpoint = "localhost:50051"
"#;
        let result: Result<ConfigFile, _> = toml::from_str(toml);
        assert!(result.is_err());
    }
}
