use std::path::Path;
use std::process::ExitCode;

use crate::metrics::BenchmarkReport;
use crate::report::print_report;

/// Load two saved JSON reports and print a comparison.
pub fn compare_reports(path_a: &Path, path_b: &Path) -> ExitCode {
    let report_a = match load_report(path_a) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading {}: {e}", path_a.display());
            return ExitCode::FAILURE;
        }
    };
    let report_b = match load_report(path_b) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading {}: {e}", path_b.display());
            return ExitCode::FAILURE;
        }
    };

    // Merge backends from both reports into a combined report for comparison display.
    let mut combined = report_a.clone();
    combined.backends.extend(report_b.backends.iter().cloned());
    combined.duration_secs = report_a.duration_secs.max(report_b.duration_secs);
    combined.records.extend(report_b.records.iter().cloned());

    print_report(&combined);

    ExitCode::SUCCESS
}

fn load_report(path: &Path) -> Result<BenchmarkReport, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let report: BenchmarkReport = serde_json::from_str(&content)?;
    Ok(report)
}
