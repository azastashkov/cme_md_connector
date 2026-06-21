//! `connector` — runs the CME market-data load test with a live latency dashboard.

use std::collections::HashMap;
use std::time::Duration;

use cme_md_connector::loadgen::GeneratorConfig;
use cme_md_connector::pipeline::PipelineConfig;
use cme_md_connector::risk::RiskConfig;
use cme_md_connector::runner::{self, RunConfig};
use cme_md_connector::signal::SignalConfig;

const HELP: &str = "\
cme_md_connector — HFT CME (MDP 3.0 / SBE) market-data connector + load test

USAGE:
  connector [OPTIONS]

LOAD TEST:
  --rate <pps>           open-loop packet rate; 0 = unthrottled  [default: 200000]
  --duration <secs>      run length in seconds                   [default: 30]
  --instruments <n>      number of synthetic instruments         [default: 4]
  --ring <slots>         ingest ring capacity                    [default: 16384]
  --seed <u64>           generator PRNG seed                     [default: 12877…]
  --pcap <path>          replay a local CME pcap instead of generating

DASHBOARD:
  --port <p>             dashboard HTTP port                     [default: 8080]
  --no-dashboard         disable the built-in dashboard
  --calibrate            micro-benchmark per-stage cost and exit

MARKET MODEL:
  --base-price <f>       OU mean / start mid                     [default: 5000.0]
  --tick <f>             tick size                               [default: 0.25]
  --theta <f>            OU mean-reversion speed                 [default: 0.05]
  --sigma <f>            OU shock scale                          [default: 0.5]

STRATEGY (mean-reversion z-score):
  --alpha <f>            EWMA smoothing                          [default: 0.1]
  --z-entry <f>          entry threshold                         [default: 2.0]
  --z-exit <f>           exit threshold                          [default: 0.5]
  --z-stop <f>           stop threshold                          [default: 3.0]

RISK:
  --order-qty <n>        contracts per order                     [default: 1]
  --multiplier <f>       contract multiplier                     [default: 50.0]
  --pos-limit <n>        max |net position| per instrument       [default: 100]
  --loss-limit <f>       daily loss cap                          [default: 10000]
  --price-band-ticks <n> fat-finger price band (ticks)           [default: 20]

  -h, --help             print this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return;
    }

    let (flags, opts) = match parse_args(&args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}\n\nrun `connector --help` for usage.");
            std::process::exit(2);
        }
    };

    let instruments_n: i32 = opts.get_or("instruments", 4);
    let instruments: Vec<i32> = (1..=instruments_n).collect();

    let signal = SignalConfig {
        alpha: opts.get_or("alpha", 0.1),
        z_entry: opts.get_or("z-entry", 2.0),
        z_exit: opts.get_or("z-exit", 0.5),
        z_stop: opts.get_or("z-stop", 3.0),
        warmup: opts.get_or("warmup", 50),
    };
    let tick: f64 = opts.get_or("tick", 0.25);
    let risk = RiskConfig {
        position_limit: opts.get_or("pos-limit", 100),
        daily_loss_limit: opts.get_or("loss-limit", 10_000.0),
        price_band_ticks: opts.get_or("price-band-ticks", 20),
        tick,
    };
    let multiplier: f64 = opts.get_or("multiplier", 50.0);

    let pipeline = PipelineConfig {
        instruments: instruments.clone(),
        multiplier,
        order_qty: opts.get_or("order-qty", 1),
        signal,
        risk,
    };
    let generator = GeneratorConfig {
        instruments,
        base_price: opts.get_or("base-price", 5000.0),
        tick,
        half_spread_ticks: 1,
        theta: opts.get_or("theta", 0.05),
        sigma: opts.get_or("sigma", 0.5),
        size: opts.get_or("size", 10),
        seed: opts.get_or("seed", 0xC0FF_EE12_3456_789A_u64),
    };

    // Optional replay of a locally-supplied real CME capture.
    let replay = match opts.map.get("pcap") {
        Some(path) => match cme_md_connector::loadgen::read_pcap_payloads(path) {
            Ok(payloads) if !payloads.is_empty() => {
                println!("Loaded {} UDP payload(s) from {path}.", payloads.len());
                Some(std::sync::Arc::new(payloads))
            }
            Ok(_) => {
                eprintln!("error: no UDP payloads found in {path}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: failed to read pcap {path}: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let cfg = RunConfig {
        pipeline,
        generator,
        rate: opts.get_or("rate", 200_000),
        replay,
        duration: Duration::from_secs(opts.get_or("duration", 30)),
        port: opts.get_or("port", 8080),
        dashboard: !flags.iter().any(|f| f == "no-dashboard"),
        ring_capacity: opts.get_or("ring", 16_384),
        report_interval: Duration::from_secs(1),
    };

    if flags.iter().any(|f| f == "calibrate") {
        println!("Calibrating per-stage cost (isolated micro-benchmarks)…");
        for (label, mean_ns) in runner::calibrate(&cfg, 200_000) {
            println!("  {label:<30} {mean_ns:>8.1} ns/iter");
        }
        return;
    }

    println!(
        "Starting load test: {} instrument(s), rate {} pkt/s, {}s.",
        cfg.pipeline.instruments.len(),
        cfg.rate,
        cfg.duration.as_secs()
    );
    if cfg.dashboard {
        println!("Dashboard:  http://127.0.0.1:{}/", cfg.port);
    }
    println!("Running…");

    let result = runner::run(cfg);
    println!("{}", runner::format_report(&result));
}

/// Parsed flags (boolean `--x`) and options (`--k v`).
struct Opts {
    map: HashMap<String, String>,
}

impl Opts {
    fn get_or<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.map
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}

fn parse_args(args: &[String]) -> Result<(Vec<String>, Opts), String> {
    const BOOL_FLAGS: &[&str] = &["no-dashboard", "calibrate"];
    let mut flags = Vec::new();
    let mut map = HashMap::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let key = arg
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument `{arg}`"))?;
        if BOOL_FLAGS.contains(&key) {
            flags.push(key.to_string());
            i += 1;
        } else {
            let value = args
                .get(i + 1)
                .ok_or_else(|| format!("missing value for `--{key}`"))?;
            map.insert(key.to_string(), value.clone());
            i += 2;
        }
    }
    Ok((flags, Opts { map }))
}
