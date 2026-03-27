use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod bench;
mod compare;
mod config;
mod metrics;
mod report;
mod tokens;

use config::{BackendDef, BenchConfig, FrontendDef, TransportDef};

#[derive(Parser)]
#[command(
    name = "turbobench",
    version,
    about = "MCP server benchmarking proxy — measure latency, token usage, and compare MCP servers"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Backend name (for reports)
    #[arg(long, short = 'n')]
    name: Option<String>,

    /// Save report to JSON file
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,

    /// Suppress report output on stderr
    #[arg(long, short)]
    quiet: bool,

    /// Config file (TOML) for advanced/dual-backend configuration
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// Backend HTTP URL (instead of stdio command)
    #[arg(long)]
    url: Option<String>,

    /// Frontend type: stdio (default) or http
    #[arg(long, default_value = "stdio")]
    frontend: String,

    /// Frontend bind address (for HTTP frontend)
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: String,

    /// Backend command and arguments (after --)
    #[arg(last = true)]
    backend: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Compare two saved benchmark reports
    Compare {
        /// First report JSON file
        report_a: PathBuf,
        /// Second report JSON file
        report_b: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // Subcommands
    if let Some(Commands::Compare { report_a, report_b }) = cli.command {
        return compare::compare_reports(&report_a, &report_b);
    }

    // Warn about conflicting args
    if cli.config.is_some() {
        if cli.url.is_some() {
            eprintln!("[turbobench] warning: --url is ignored when --config is provided");
        }
        if !cli.backend.is_empty() {
            eprintln!("[turbobench] warning: positional backend args are ignored when --config is provided");
        }
    }

    // Build config
    let bench_config = if let Some(ref config_path) = cli.config {
        match config::load_config(config_path) {
            Ok(mut c) => {
                // CLI overrides
                if let Some(ref o) = cli.output {
                    c.output = Some(o.clone());
                }
                if cli.quiet {
                    c.quiet = true;
                }
                c
            }
            Err(e) => {
                eprintln!("Error loading config: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else if let Some(ref url) = cli.url {
        // HTTP backend mode
        BenchConfig {
            primary: BackendDef {
                name: cli.name,
                transport: TransportDef::Http {
                    url: url.clone(),
                    auth_token: None,
                },
            },
            shadow: None,
            frontend: parse_frontend(&cli.frontend, &cli.bind),
            output: cli.output,
            quiet: cli.quiet,
        }
    } else if cli.backend.is_empty() {
        eprintln!("Error: no backend specified.");
        eprintln!();
        eprintln!("Usage:");
        eprintln!("  turbobench -- <command> [args...]     # stdio backend");
        eprintln!("  turbobench --url http://host:port     # HTTP backend");
        eprintln!("  turbobench -c config.toml             # config file");
        eprintln!("  turbobench compare a.json b.json      # compare reports");
        return ExitCode::FAILURE;
    } else {
        // Stdio backend from positional args
        let command = cli.backend[0].clone();
        let args = cli.backend[1..].to_vec();
        BenchConfig {
            primary: BackendDef {
                name: cli.name,
                transport: TransportDef::Stdio {
                    command,
                    args,
                    working_dir: None,
                },
            },
            shadow: None,
            frontend: parse_frontend(&cli.frontend, &cli.bind),
            output: cli.output,
            quiet: cli.quiet,
        }
    };

    // Validate output path early so we don't lose a session to a bad path
    if let Some(ref path) = bench_config.output {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                eprintln!("Error: output directory does not exist: {}", parent.display());
                return ExitCode::FAILURE;
            }
        }
    }

    // Create and run proxy
    let proxy = match bench::BenchProxy::new(&bench_config).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to connect to backend: {e}");
            return ExitCode::FAILURE;
        }
    };

    match proxy.run(&bench_config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Proxy error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_frontend(frontend: &str, bind: &str) -> FrontendDef {
    match frontend {
        "http" => FrontendDef::Http {
            bind: bind.to_string(),
        },
        _ => FrontendDef::Stdio,
    }
}
