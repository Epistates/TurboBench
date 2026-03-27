use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

use crate::tokens::estimate_tokens;

/// Maximum records before we stop collecting (prevents unbounded memory growth).
const MAX_RECORDS: usize = 500_000;

/// A pending request awaiting a response (for raw passthrough correlation).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingRequest {
    pub start: Instant,
    pub method: String,
    pub tool_name: Option<String>,
    pub resource_uri: Option<String>,
    pub prompt_name: Option<String>,
    pub request_bytes: usize,
}

/// A single recorded call through the proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub backend: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_name: Option<String>,
    pub latency_us: u64,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub estimated_input_tokens: usize,
    pub estimated_output_tokens: usize,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Latency percentile statistics.
///
/// Uses **sample** standard deviation (Bessel's correction, N-1 denominator).
/// Percentiles use linear interpolation between adjacent ranks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub std_dev_ms: f64,
}

impl LatencyStats {
    /// Compute statistics from a mutable slice of latencies in microseconds.
    /// Returns `None` if the slice is empty.
    pub fn from_latencies_us(latencies: &mut [u64]) -> Option<Self> {
        if latencies.is_empty() {
            return None;
        }
        latencies.sort_unstable();
        let n = latencies.len();
        let sum: u64 = latencies.iter().sum();
        let mean = sum as f64 / n as f64;

        // Sample variance (Bessel's correction: divide by N-1 for N>1)
        let variance = if n > 1 {
            latencies
                .iter()
                .map(|&x| {
                    let d = x as f64 - mean;
                    d * d
                })
                .sum::<f64>()
                / (n - 1) as f64
        } else {
            0.0
        };

        Some(Self {
            count: n,
            min_ms: latencies[0] as f64 / 1000.0,
            max_ms: latencies[n - 1] as f64 / 1000.0,
            mean_ms: mean / 1000.0,
            p50_ms: interpolated_percentile(latencies, 50.0) / 1000.0,
            p95_ms: interpolated_percentile(latencies, 95.0) / 1000.0,
            p99_ms: interpolated_percentile(latencies, 99.0) / 1000.0,
            std_dev_ms: variance.sqrt() / 1000.0,
        })
    }
}

/// Linear interpolation percentile (same method as NumPy's default).
fn interpolated_percentile(sorted: &[u64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    if sorted.len() == 1 {
        return sorted[0] as f64;
    }
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    sorted[lo] as f64 * (1.0 - frac) + sorted[hi] as f64 * frac
}

/// Per-tool summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub call_count: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub total_input_bytes: usize,
    pub total_output_bytes: usize,
    pub estimated_input_tokens: usize,
    pub estimated_output_tokens: usize,
    pub latency: Option<LatencyStats>,
}

/// Per-method summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodSummary {
    pub method: String,
    pub call_count: usize,
    pub total_input_bytes: usize,
    pub total_output_bytes: usize,
    pub estimated_input_tokens: usize,
    pub estimated_output_tokens: usize,
    pub latency: Option<LatencyStats>,
}

/// Aggregate summary for one backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSummary {
    pub name: String,
    pub total_calls: usize,
    pub total_tool_calls: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub success_rate: f64,
    pub total_input_bytes: usize,
    pub total_output_bytes: usize,
    pub total_bytes: usize,
    pub estimated_input_tokens: usize,
    pub estimated_output_tokens: usize,
    pub estimated_total_tokens: usize,
    pub overall_latency: Option<LatencyStats>,
    pub tool_call_latency: Option<LatencyStats>,
    pub tools: Vec<ToolSummary>,
    pub methods: Vec<MethodSummary>,
}

/// Full benchmark report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_secs: f64,
    pub backends: Vec<BackendSummary>,
    pub records: Vec<CallRecord>,
}

/// Collects `CallRecord`s during a session and generates reports.
pub struct MetricsStore {
    pub records: Vec<CallRecord>,
    pub session_start: Instant,
    pub session_start_utc: DateTime<Utc>,
    dropped: usize,
}

impl MetricsStore {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            session_start: Instant::now(),
            session_start_utc: Utc::now(),
            dropped: 0,
        }
    }

    pub fn record(&mut self, rec: CallRecord) {
        if self.records.len() >= MAX_RECORDS {
            self.dropped += 1;
            return;
        }
        self.records.push(rec);
    }

    /// Build a `CallRecord` from timing and result info.
    pub fn build_record(
        backend: &str,
        method: &str,
        tool_name: Option<&str>,
        resource_uri: Option<&str>,
        prompt_name: Option<&str>,
        start: Instant,
        request_bytes: usize,
        result: &Result<serde_json::Value, String>,
    ) -> CallRecord {
        let latency = start.elapsed();
        let (success, response_bytes, error_msg) = match result {
            Ok(v) => (
                true,
                serde_json::to_string(v).unwrap_or_default().len(),
                None,
            ),
            Err(e) => (false, 0, Some(e.clone())),
        };

        CallRecord {
            backend: backend.to_string(),
            method: method.to_string(),
            tool_name: tool_name.map(String::from),
            resource_uri: resource_uri.map(String::from),
            prompt_name: prompt_name.map(String::from),
            latency_us: latency.as_micros().min(u128::from(u64::MAX)) as u64,
            request_bytes,
            response_bytes,
            estimated_input_tokens: estimate_tokens(request_bytes),
            estimated_output_tokens: estimate_tokens(response_bytes),
            success,
            error_message: error_msg,
            timestamp: Utc::now(),
        }
    }

    /// Generate a full report from collected records.
    pub fn generate_report(&self, session_id: &str) -> BenchmarkReport {
        if self.dropped > 0 {
            tracing::warn!("{} records dropped (limit: {MAX_RECORDS})", self.dropped);
        }

        let now = Utc::now();
        let duration = self.session_start.elapsed();

        let mut by_backend: HashMap<String, Vec<&CallRecord>> = HashMap::new();
        for r in &self.records {
            by_backend.entry(r.backend.clone()).or_default().push(r);
        }

        let mut backends: Vec<BackendSummary> = by_backend
            .into_iter()
            .map(|(name, recs)| Self::summarize(&name, &recs))
            .collect();
        backends.sort_by(|a, b| a.name.cmp(&b.name));

        BenchmarkReport {
            session_id: session_id.to_string(),
            started_at: self.session_start_utc,
            ended_at: now,
            duration_secs: duration.as_secs_f64(),
            backends,
            records: self.records.clone(),
        }
    }

    fn summarize(name: &str, records: &[&CallRecord]) -> BackendSummary {
        let total_calls = records.len();
        let success_count = records.iter().filter(|r| r.success).count();
        let failure_count = total_calls - success_count;
        let success_rate = if total_calls > 0 {
            success_count as f64 / total_calls as f64 * 100.0
        } else {
            0.0
        };

        let total_input_bytes: usize = records.iter().map(|r| r.request_bytes).sum();
        let total_output_bytes: usize = records.iter().map(|r| r.response_bytes).sum();
        let est_in: usize = records.iter().map(|r| r.estimated_input_tokens).sum();
        let est_out: usize = records.iter().map(|r| r.estimated_output_tokens).sum();

        let tool_calls: Vec<_> = records.iter().filter(|r| r.method == "tools/call").collect();
        let total_tool_calls = tool_calls.len();

        let mut all_lat: Vec<u64> = records.iter().map(|r| r.latency_us).collect();
        let overall_latency = LatencyStats::from_latencies_us(&mut all_lat);

        let mut tc_lat: Vec<u64> = tool_calls.iter().map(|r| r.latency_us).collect();
        let tool_call_latency = LatencyStats::from_latencies_us(&mut tc_lat);

        // Per-tool breakdown
        let mut tool_map: HashMap<String, Vec<&&CallRecord>> = HashMap::new();
        for r in &tool_calls {
            if let Some(ref tn) = r.tool_name {
                tool_map.entry(tn.clone()).or_default().push(r);
            }
        }
        let mut tools: Vec<ToolSummary> = tool_map
            .into_iter()
            .map(|(tn, recs)| {
                let mut lats: Vec<u64> = recs.iter().map(|r| r.latency_us).collect();
                ToolSummary {
                    name: tn,
                    call_count: recs.len(),
                    success_count: recs.iter().filter(|r| r.success).count(),
                    failure_count: recs.iter().filter(|r| !r.success).count(),
                    total_input_bytes: recs.iter().map(|r| r.request_bytes).sum(),
                    total_output_bytes: recs.iter().map(|r| r.response_bytes).sum(),
                    estimated_input_tokens: recs.iter().map(|r| r.estimated_input_tokens).sum(),
                    estimated_output_tokens: recs.iter().map(|r| r.estimated_output_tokens).sum(),
                    latency: LatencyStats::from_latencies_us(&mut lats),
                }
            })
            .collect();
        tools.sort_by(|a, b| b.call_count.cmp(&a.call_count));

        // Per-method breakdown
        let mut method_map: HashMap<String, Vec<&CallRecord>> = HashMap::new();
        for r in records {
            method_map.entry(r.method.clone()).or_default().push(r);
        }
        let mut methods: Vec<MethodSummary> = method_map
            .into_iter()
            .map(|(m, recs)| {
                let mut lats: Vec<u64> = recs.iter().map(|r| r.latency_us).collect();
                MethodSummary {
                    method: m,
                    call_count: recs.len(),
                    total_input_bytes: recs.iter().map(|r| r.request_bytes).sum(),
                    total_output_bytes: recs.iter().map(|r| r.response_bytes).sum(),
                    estimated_input_tokens: recs.iter().map(|r| r.estimated_input_tokens).sum(),
                    estimated_output_tokens: recs.iter().map(|r| r.estimated_output_tokens).sum(),
                    latency: LatencyStats::from_latencies_us(&mut lats),
                }
            })
            .collect();
        methods.sort_by(|a, b| b.call_count.cmp(&a.call_count));

        BackendSummary {
            name: name.to_string(),
            total_calls,
            total_tool_calls,
            success_count,
            failure_count,
            success_rate,
            total_input_bytes,
            total_output_bytes,
            total_bytes: total_input_bytes + total_output_bytes,
            estimated_input_tokens: est_in,
            estimated_output_tokens: est_out,
            estimated_total_tokens: est_in + est_out,
            overall_latency,
            tool_call_latency,
            tools,
            methods,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_stats_empty() {
        assert!(LatencyStats::from_latencies_us(&mut []).is_none());
    }

    #[test]
    fn latency_stats_single() {
        let stats = LatencyStats::from_latencies_us(&mut [5000]).unwrap();
        assert_eq!(stats.count, 1);
        assert!((stats.min_ms - 5.0).abs() < f64::EPSILON);
        assert!((stats.max_ms - 5.0).abs() < f64::EPSILON);
        assert!((stats.p50_ms - 5.0).abs() < f64::EPSILON);
        assert!((stats.p95_ms - 5.0).abs() < f64::EPSILON);
        assert!((stats.p99_ms - 5.0).abs() < f64::EPSILON);
        assert!((stats.std_dev_ms).abs() < f64::EPSILON); // N=1 => std_dev=0
    }

    #[test]
    fn latency_stats_two_values() {
        let stats = LatencyStats::from_latencies_us(&mut [1000, 3000]).unwrap();
        assert_eq!(stats.count, 2);
        assert!((stats.min_ms - 1.0).abs() < f64::EPSILON);
        assert!((stats.max_ms - 3.0).abs() < f64::EPSILON);
        assert!((stats.mean_ms - 2.0).abs() < f64::EPSILON);
        // p50 with interpolation: rank = 0.5*1 = 0.5, interp(1000,3000,0.5) = 2000
        assert!((stats.p50_ms - 2.0).abs() < f64::EPSILON);
        // Sample std_dev for [1000,3000]: sqrt((1e6+1e6)/1) = sqrt(2e6) ≈ 1414.21 us = 1.414 ms
        assert!((stats.std_dev_ms - 1.4142135623730951).abs() < 0.001);
    }

    #[test]
    fn latency_stats_known_dataset() {
        // 10 values: 100, 200, ..., 1000 us
        let mut data: Vec<u64> = (1..=10).map(|i| i * 100).collect();
        let stats = LatencyStats::from_latencies_us(&mut data).unwrap();
        assert_eq!(stats.count, 10);
        assert!((stats.min_ms - 0.1).abs() < f64::EPSILON);
        assert!((stats.max_ms - 1.0).abs() < f64::EPSILON);
        assert!((stats.mean_ms - 0.55).abs() < 0.001);
    }

    #[test]
    fn interpolated_percentile_linear() {
        // sorted: [10, 20, 30, 40, 50]
        let sorted = [10u64, 20, 30, 40, 50];
        // p50: rank = 0.5 * 4 = 2.0, exact => sorted[2] = 30
        assert!((interpolated_percentile(&sorted, 50.0) - 30.0).abs() < f64::EPSILON);
        // p25: rank = 0.25 * 4 = 1.0, exact => sorted[1] = 20
        assert!((interpolated_percentile(&sorted, 25.0) - 20.0).abs() < f64::EPSILON);
        // p10: rank = 0.10 * 4 = 0.4, interp(10, 20, 0.4) = 14
        assert!((interpolated_percentile(&sorted, 10.0) - 14.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_store_respects_limit() {
        let mut store = MetricsStore::new();
        let rec = CallRecord {
            backend: "test".into(),
            method: "tools/call".into(),
            tool_name: None,
            resource_uri: None,
            prompt_name: None,
            latency_us: 100,
            request_bytes: 10,
            response_bytes: 20,
            estimated_input_tokens: 3,
            estimated_output_tokens: 5,
            success: true,
            error_message: None,
            timestamp: Utc::now(),
        };
        // Fill to MAX + 10
        for _ in 0..MAX_RECORDS + 10 {
            store.record(rec.clone());
        }
        assert_eq!(store.records.len(), MAX_RECORDS);
        assert_eq!(store.dropped, 10);
    }

    #[test]
    fn build_record_success() {
        let start = Instant::now();
        let val = serde_json::json!({"ok": true});
        let rec = MetricsStore::build_record(
            "backend", "tools/call", Some("click"), None, None,
            start, 50, &Ok(val),
        );
        assert!(rec.success);
        assert!(rec.error_message.is_none());
        assert!(rec.response_bytes > 0);
        assert!(rec.latency_us < 1_000_000); // < 1s
    }

    #[test]
    fn build_record_error() {
        let start = Instant::now();
        let rec = MetricsStore::build_record(
            "backend", "tools/call", Some("click"), None, None,
            start, 50, &Err("timeout".into()),
        );
        assert!(!rec.success);
        assert_eq!(rec.error_message.as_deref(), Some("timeout"));
        assert_eq!(rec.response_bytes, 0);
    }

    #[test]
    fn generate_report_empty() {
        let store = MetricsStore::new();
        let report = store.generate_report("test-session");
        assert!(report.backends.is_empty());
        assert!(report.records.is_empty());
        assert!(report.duration_secs >= 0.0);
    }
}
