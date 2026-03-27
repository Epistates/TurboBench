use std::path::Path;

use colored::Colorize;
use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

use crate::metrics::{BackendSummary, BenchmarkReport, LatencyStats};

/// Print a formatted benchmark report to stderr.
pub fn print_report(report: &BenchmarkReport) {
    let out = std::io::stderr();
    let is_tty = std::io::IsTerminal::is_terminal(&out);
    if !is_tty {
        colored::control::set_override(false);
    }

    // Safe slice — handle short session IDs from user-supplied report files.
    let sid = if report.session_id.len() >= 8 {
        &report.session_id[..8]
    } else {
        &report.session_id
    };

    eprintln!();
    eprintln!(
        "{}",
        "=== TurboBench Report ===".bold().cyan()
    );
    eprintln!("  Session:  {}", sid);
    eprintln!(
        "  Duration: {:.1}s",
        report.duration_secs
    );
    eprintln!(
        "  Records:  {}",
        report.records.len()
    );
    eprintln!();

    for backend in &report.backends {
        print_backend(backend);
    }

    if report.backends.len() > 1 {
        print_comparison(&report.backends);
    }
}

fn print_backend(b: &BackendSummary) {
    eprintln!("{}", format!("--- {} ---", b.name).bold());

    // Overview table
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED);
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t.set_header(vec!["Metric", "Value"]);

    t.add_row(vec!["Total calls", &fmt_num(b.total_calls)]);
    t.add_row(vec!["Tool calls", &fmt_num(b.total_tool_calls)]);
    t.add_row(vec![
        "Success rate",
        &format!("{:.1}%", b.success_rate),
    ]);
    t.add_row(vec![
        "Total bytes (in/out)",
        &format!(
            "{} / {}",
            fmt_bytes(b.total_input_bytes),
            fmt_bytes(b.total_output_bytes)
        ),
    ]);
    t.add_row(vec![
        "Est. tokens (in/out)",
        &format!(
            "~{} / ~{}",
            fmt_num(b.estimated_input_tokens),
            fmt_num(b.estimated_output_tokens)
        ),
    ]);
    t.add_row(vec![
        "Est. total tokens",
        &format!("~{}", fmt_num(b.estimated_total_tokens)),
    ]);

    if let Some(ref lat) = b.overall_latency {
        t.add_row(vec![
            "Latency (mean/p50/p95/p99)",
            &format_latency_inline(lat),
        ]);
    }

    eprintln!("{t}");
    eprintln!();

    // Per-tool table
    if !b.tools.is_empty() {
        let mut tt = Table::new();
        tt.load_preset(UTF8_FULL_CONDENSED);
        tt.set_content_arrangement(ContentArrangement::Dynamic);
        tt.set_header(vec![
            "Tool",
            "Calls",
            "OK%",
            "Tokens(in)",
            "Tokens(out)",
            "P50ms",
            "P95ms",
            "P99ms",
        ]);

        for tool in &b.tools {
            let ok_pct = if tool.call_count > 0 {
                format!("{:.0}%", tool.success_count as f64 / tool.call_count as f64 * 100.0)
            } else {
                "-".to_string()
            };
            let (p50, p95, p99) = match &tool.latency {
                Some(l) => (
                    format!("{:.0}", l.p50_ms),
                    format!("{:.0}", l.p95_ms),
                    format!("{:.0}", l.p99_ms),
                ),
                None => ("-".into(), "-".into(), "-".into()),
            };
            tt.add_row(vec![
                &tool.name,
                &fmt_num(tool.call_count),
                &ok_pct,
                &format!("~{}", fmt_num(tool.estimated_input_tokens)),
                &format!("~{}", fmt_num(tool.estimated_output_tokens)),
                &p50,
                &p95,
                &p99,
            ]);
        }
        eprintln!("{tt}");
        eprintln!();
    }

    // Per-method table
    if !b.methods.is_empty() {
        let mut mt = Table::new();
        mt.load_preset(UTF8_FULL_CONDENSED);
        mt.set_content_arrangement(ContentArrangement::Dynamic);
        mt.set_header(vec![
            "Method",
            "Calls",
            "Bytes(in)",
            "Bytes(out)",
            "Mean ms",
        ]);

        for m in &b.methods {
            let mean = m
                .latency
                .as_ref()
                .map(|l| format!("{:.1}", l.mean_ms))
                .unwrap_or_else(|| "-".into());
            mt.add_row(vec![
                &m.method,
                &fmt_num(m.call_count),
                &fmt_bytes(m.total_input_bytes),
                &fmt_bytes(m.total_output_bytes),
                &mean,
            ]);
        }
        eprintln!("{mt}");
        eprintln!();
    }
}

fn print_comparison(backends: &[BackendSummary]) {
    if backends.len() < 2 {
        return;
    }
    let a = &backends[0];
    let b = &backends[1];

    eprintln!(
        "{}",
        "=== Comparison ===".bold().yellow()
    );

    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED);
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t.set_header(vec!["Metric", &a.name, &b.name, "Delta"]);

    t.add_row(vec![
        "Total calls".to_string(),
        fmt_num(a.total_calls),
        fmt_num(b.total_calls),
        delta_pct(a.total_calls as f64, b.total_calls as f64),
    ]);
    t.add_row(vec![
        "Success rate".to_string(),
        format!("{:.1}%", a.success_rate),
        format!("{:.1}%", b.success_rate),
        delta_abs(a.success_rate, b.success_rate, "%"),
    ]);
    t.add_row(vec![
        "Est. total tokens".to_string(),
        format!("~{}", fmt_num(a.estimated_total_tokens)),
        format!("~{}", fmt_num(b.estimated_total_tokens)),
        delta_pct(a.estimated_total_tokens as f64, b.estimated_total_tokens as f64),
    ]);
    t.add_row(vec![
        "Total bytes".to_string(),
        fmt_bytes(a.total_bytes),
        fmt_bytes(b.total_bytes),
        delta_pct(a.total_bytes as f64, b.total_bytes as f64),
    ]);

    if let (Some(la), Some(lb)) = (&a.overall_latency, &b.overall_latency) {
        t.add_row(vec![
            "Mean latency".to_string(),
            format!("{:.1}ms", la.mean_ms),
            format!("{:.1}ms", lb.mean_ms),
            delta_pct(la.mean_ms, lb.mean_ms),
        ]);
        t.add_row(vec![
            "P95 latency".to_string(),
            format!("{:.1}ms", la.p95_ms),
            format!("{:.1}ms", lb.p95_ms),
            delta_pct(la.p95_ms, lb.p95_ms),
        ]);
    }

    eprintln!("{t}");
    eprintln!();
}

/// Save the report as JSON (atomic write via temp file + rename).
pub fn save_report(
    report: &BenchmarkReport,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    if !parent.exists() {
        return Err(format!("Directory does not exist: {}", parent.display()).into());
    }

    let json = serde_json::to_string_pretty(report)?;
    let tmp_path = path.with_extension("tmp");

    // Write to a temp file, then atomically rename.
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, path)?;

    eprintln!("[turbobench] report saved to {}", path.display());
    Ok(())
}

// --- Formatting helpers ---

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

fn fmt_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn format_latency_inline(l: &LatencyStats) -> String {
    format!(
        "{:.0} / {:.0} / {:.0} / {:.0} ms",
        l.mean_ms, l.p50_ms, l.p95_ms, l.p99_ms
    )
}

fn delta_pct(a: f64, b: f64) -> String {
    if a == 0.0 {
        return "-".to_string();
    }
    let pct = (b - a) / a * 100.0;
    if pct.abs() < 0.5 {
        "~0%".to_string()
    } else if pct > 0.0 {
        format!("+{:.0}%", pct)
    } else {
        format!("{:.0}%", pct)
    }
}

fn delta_abs(a: f64, b: f64, unit: &str) -> String {
    let d = b - a;
    if d.abs() < 0.05 {
        format!("~0{unit}")
    } else if d > 0.0 {
        format!("+{:.1}{unit}", d)
    } else {
        format!("{:.1}{unit}", d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_num() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1000), "1,000");
        assert_eq!(fmt_num(1_000_000), "1,000,000");
        assert_eq!(fmt_num(12345), "12,345");
    }

    #[test]
    fn test_fmt_bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn test_delta_pct() {
        assert_eq!(delta_pct(0.0, 100.0), "-");
        assert_eq!(delta_pct(100.0, 100.0), "~0%");
        assert_eq!(delta_pct(100.0, 200.0), "+100%");
        assert_eq!(delta_pct(200.0, 100.0), "-50%");
    }

    #[test]
    fn test_delta_abs() {
        assert_eq!(delta_abs(50.0, 50.0, "%"), "~0%");
        assert_eq!(delta_abs(50.0, 55.1, "%"), "+5.1%");
        assert_eq!(delta_abs(55.1, 50.0, "%"), "-5.1%");
    }

    #[test]
    fn test_safe_session_id_slice() {
        // Simulate a short session_id from a hand-crafted report
        let report = BenchmarkReport {
            session_id: "abc".to_string(),
            started_at: chrono::Utc::now(),
            ended_at: chrono::Utc::now(),
            duration_secs: 0.0,
            backends: vec![],
            records: vec![],
        };
        // Should not panic
        print_report(&report);
    }
}
