//! Built-in dashboard: serializes [`MetricsSnapshot`] to JSON and serves the
//! embedded HTML page plus a `/metrics.json` endpoint over `tiny_http`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tiny_http::{Header, Response, Server};

use super::{MetricsSnapshot, StageStat};

const INDEX_HTML: &str = include_str!("../../assets/dashboard.html");

fn stages_json(stages: &[(&'static str, StageStat)]) -> String {
    let items: Vec<String> = stages
        .iter()
        .map(|(name, s)| {
            format!(
                "{{\"stage\":\"{}\",\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{},\"count\":{}}}",
                name, s.p50, s.p95, s.p99, s.max, s.count
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Serialize a snapshot to compact JSON (no serde dependency).
pub fn to_json(snap: &MetricsSnapshot) -> String {
    format!(
        "{{\"throughput_pps\":{:.3},\"ticks\":{},\"orders\":{},\"rejects\":{},\"drops\":{},\
\"timer_resolution_ns\":{},\"total_pnl\":{:.4},\"net_position\":{},\"interval\":{},\"cumulative\":{}}}",
        snap.throughput_pps,
        snap.ticks,
        snap.orders,
        snap.rejects,
        snap.drops,
        snap.timer_resolution_ns,
        snap.total_pnl,
        snap.net_position,
        stages_json(&snap.interval),
        stages_json(&snap.cumulative),
    )
}

fn header(content_type: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).expect("valid header")
}

/// Run the dashboard HTTP server until `shutdown` is set. Blocks the calling
/// thread; intended to run on its own thread.
pub fn serve(
    port: u16,
    latest: Arc<ArcSwap<MetricsSnapshot>>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    while !shutdown.load(Ordering::Relaxed) {
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => {
                let response = if req.url().starts_with("/metrics.json") {
                    let snap = latest.load();
                    Response::from_string(to_json(&snap)).with_header(header("application/json"))
                } else {
                    Response::from_string(INDEX_HTML).with_header(header("text/html; charset=utf-8"))
                };
                let _ = req.respond(response);
            }
            Ok(None) => {} // timeout — re-check the shutdown flag
            Err(_) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            interval: vec![(
                "decode",
                StageStat {
                    p50: 100,
                    p95: 200,
                    p99: 300,
                    max: 400,
                    count: 10,
                },
            )],
            cumulative: vec![(
                "decode",
                StageStat {
                    p50: 110,
                    p95: 210,
                    p99: 310,
                    max: 410,
                    count: 100,
                },
            )],
            throughput_pps: 12345.678,
            ticks: 100,
            orders: 5,
            rejects: 2,
            drops: 0,
            timer_resolution_ns: 42,
            total_pnl: -123.45,
            net_position: -7,
            ..Default::default()
        }
    }

    #[test]
    fn json_includes_stage_names_and_top_level_fields() {
        let json = to_json(&sample_snapshot());
        assert!(json.contains("\"stage\":\"decode\""));
        assert!(json.contains("\"throughput_pps\":12345.678"));
        assert!(json.contains("\"timer_resolution_ns\":42"));
        assert!(json.contains("\"total_pnl\":-123.4500"));
        assert!(json.contains("\"net_position\":-7"));
        assert!(json.contains("\"p99\":300"));
        assert!(json.contains("\"interval\""));
        assert!(json.contains("\"cumulative\""));
    }

    #[test]
    fn index_html_includes_pnl_and_position_charts() {
        // Guards against the embedded asset drifting away from the PnL and
        // position graphs (there is no JS test harness, so assert the canvases
        // and the shared signed renderer are present).
        assert!(
            INDEX_HTML.contains("id=\"c_pnl\""),
            "dashboard.html is missing the PnL chart canvas"
        );
        assert!(
            INDEX_HTML.contains("id=\"c_pos\""),
            "dashboard.html is missing the position chart canvas"
        );
        assert!(
            INDEX_HTML.contains("drawSigned"),
            "dashboard.html is missing the shared drawSigned renderer"
        );
    }

    #[test]
    fn json_is_balanced_braces() {
        let json = to_json(&sample_snapshot());
        let open = json.chars().filter(|&c| c == '{').count();
        let close = json.chars().filter(|&c| c == '}').count();
        assert_eq!(open, close, "unbalanced braces in {json}");
    }
}
