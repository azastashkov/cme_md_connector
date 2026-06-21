//! End-to-end test of the full load-test pipeline through the public API.

use std::time::Duration;

use cme_md_connector::loadgen::GeneratorConfig;
use cme_md_connector::metrics::dashboard::to_json;
use cme_md_connector::pipeline::PipelineConfig;
use cme_md_connector::risk::RiskConfig;
use cme_md_connector::runner::{run, RunConfig};
use cme_md_connector::signal::SignalConfig;

fn config(duration_ms: u64) -> RunConfig {
    let instruments = vec![1, 2, 3, 4];
    RunConfig {
        pipeline: PipelineConfig {
            instruments: instruments.clone(),
            multiplier: 50.0,
            order_qty: 1,
            signal: SignalConfig::default(),
            risk: RiskConfig::default(),
        },
        generator: GeneratorConfig {
            instruments,
            ..GeneratorConfig::default()
        },
        rate: 200_000,
        replay: None,
        duration: Duration::from_millis(duration_ms),
        port: 0,
        dashboard: false,
        ring_capacity: 16_384,
        report_interval: Duration::from_millis(50),
    }
}

#[test]
fn full_run_decodes_books_signals_and_routes_orders() {
    let result = run(config(700));
    let snap = &result.final_snapshot;

    // The connector decoded and processed packets.
    assert!(snap.ticks > 1000, "processed only {} packets", snap.ticks);

    // The strategy produced orders throughout the run (the midpoint-peg fill
    // model keeps PnL bounded, so the daily-loss kill-switch does not halt flow).
    assert!(snap.orders > 0, "no orders were generated");

    // Every measured stage is reported with samples.
    let stages: Vec<&str> = result.cumulative.iter().map(|(n, _)| *n).collect();
    for expected in [
        "decode",
        "book",
        "signal",
        "risk",
        "gateway",
        "tick_to_signal",
        "tick_to_order",
    ] {
        assert!(stages.contains(&expected), "missing stage {expected}");
    }

    // Per-tick stages saw every packet; the end-to-end total saw every order.
    let decode = result.cumulative.iter().find(|(n, _)| *n == "decode").unwrap().1;
    let order = result
        .cumulative
        .iter()
        .find(|(n, _)| *n == "tick_to_order")
        .unwrap()
        .1;
    assert_eq!(decode.count, snap.ticks);
    assert_eq!(order.count, snap.orders);
    // tick-to-order is the sum of stages, so it is at least as large as decode.
    assert!(order.p50 >= decode.p50);
}

#[test]
fn risk_layer_rejects_orders_at_zero_position_limit() {
    let mut cfg = config(400);
    cfg.pipeline.risk.position_limit = 0; // every order projects |1| > 0 -> rejected
    let result = run(cfg);
    assert!(
        result.final_snapshot.rejects > 0,
        "the risk layer should reject every order"
    );
    assert_eq!(
        result.final_snapshot.orders, 0,
        "no order should pass a zero position limit"
    );
}

#[test]
fn snapshot_serializes_to_valid_json() {
    let result = run(config(300));
    let json = to_json(&result.final_snapshot);
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.contains("\"tick_to_order\""));
    assert!(json.contains("\"timer_resolution_ns\""));
    let open = json.matches('{').count();
    let close = json.matches('}').count();
    assert_eq!(open, close);
}
