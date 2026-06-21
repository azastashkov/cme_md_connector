# cme_md_connector

A low-latency **CME market-data connector + load test** in Rust. It decodes raw
**CME MDP 3.0 / SBE** packets into a best-bid/offer order book, runs the book
through a **statistical-arbitrage (mean-reversion) signal**, gates orders through
**three pre-trade risk checks**, and ships them to a **mock order gateway** — with
**every stage timed (p50/p95/p99 + total)** on a built-in live dashboard.

The hot path is architected like an HFT pipeline — single-threaded and inline,
zero-copy decode, allocation-free, lock-free ingest hand-off, busy-spin — and
runs natively on macOS (Apple Silicon) with the most aggressive OS-level tricks
feature-gated for Linux and **off by default**.

```
 generator thread                 hot consumer thread (inline, busy-spin)
  OU price model ─► encode MDP3 ─► rtrb SPSC ring ─► pop (stamp t0)
   (open-loop, paced)               (hop is BEFORE t0)        │ decode   (t1)
                                                              │ book→BBO (t2)
                                                              │ signal   (t3)
                                                    signal? ──│ risk     (t4)
                                                     pass?  ──│ gateway ack+fill (t5)
                                                              │ record HdrHistograms
   reporter thread (~1s): snapshot → p50/p95/p99 + totals → ArcSwap
   dashboard thread: tiny_http serves the UI + /metrics.json
```

## Quick start

```bash
cargo test                 # 73 tests
cargo run --release -- --duration 30 --instruments 4 --rate 200000
# then open  http://127.0.0.1:8080/
```

The terminal prints a final per-stage percentile report; the dashboard shows the
same metrics live, refreshing each second.

### Other modes

```bash
# Isolated per-stage micro-benchmarks (resolves sub-timer-resolution costs):
cargo run --release -- --calibrate

# Replay a locally-supplied real CME capture instead of generating:
cargo run --release -- --pcap /path/to/local_sample.pcap

# Headless (no dashboard), useful for CI / quick checks:
cargo run --release -- --no-dashboard --duration 5

cargo run --release -- --help    # all flags
```

## The pipeline

| Stage | What it does |
|-------|--------------|
| **decode** | Parse the 12-byte CME packet header (seq + sending-time-ns), the `u16`+SBE-header message framing, and `MDIncrementalRefreshBook` (template 46/32) entries. Zero-copy via `zerocopy` unaligned little-endian types. |
| **book** | Apply New/Change/Delete/DeleteThru/DeleteFrom/Overlay + book-reset to a fixed-depth ladder; derive the BBO. Per-instrument `RptSeq` gap detection. |
| **signal** | Single-instrument **EWMA z-score mean-reversion**: `z = (mid - mean)/std`; enter at \|z\|≥2, exit at \|z\|≤0.5, stop at \|z\|≥3. O(1)/tick. |
| **risk** | Three sequential pre-trade checks (cheapest first): **Price Reasonableness** (tick band around mid), **Position Limit** (projected net cap), **Daily Loss Limit** (latching kill-switch on realized+unrealized PnL). |
| **gateway** | Mock gateway: assign OrderID, ack (`NEW`), immediate fill at the touch/limit; the fill feeds back into position/PnL. |

Two end-to-end totals are tracked: **tick→signal** (every packet) and
**tick→order** (full tick-to-trade, when an order is sent).

## Latency measurement

- **HdrHistogram** per stage (record ~3–6 ns), recorded on the hot thread and
  aggregated off it; interval (live) + cumulative (final) stats.
- The end-to-end stages use `record_correct` against the open-loop inter-arrival
  interval to defend against **coordinated omission**.
- `t0` is stamped at ring-pop, so the ingest hand-off is excluded from the
  measured latency.

### Platform caveats (important, honest framing)

The *architecture* is HFT-grade; *measurement resolution* is platform-bound:

- **Apple Silicon has a ~17–42 ns userspace timer floor** and **no userspace
  cycle counter** (`quanta` falls back to the ARM generic timer; TSC is x86-only).
  Stages below that floor quantize to ~0/one-bucket and are **labelled
  resolution-limited** on the dashboard. Use `--calibrate` (averages over millions
  of iterations) for true sub-floor per-stage cost, or run the Linux build.
- **No hard thread-to-core pinning on Apple Silicon** (`core_affinity` is a no-op
  there); the hot thread is scheduler-managed, so expect more tail variance than a
  pinned/isolated Linux core.
- The **Linux-x86 fast path** (`--features linux-x86-fastpath,pin-threads`) is the
  documented seam for true per-stage ns (rdtscp/`minstant`), `sched_setaffinity`
  pinning, and huge pages. It is **off by default**.

## Test data

There is **no free, legally-redistributable real CME MDP 3.0 pcap** — CME's
schema is public but raw captures are account/license-gated and the Information
License Agreement forbids redistribution. So the load test is driven by a
**synthetic but wire-faithful MDP 3.0 / SBE generator** (real packet headers, real
template IDs, real field offsets), built with the *same encoder the decoder
validates against* — a single source of truth. To validate against real data, pass
a locally-supplied capture with `--pcap` (it is parsed via `pcap-parser` +
`etherparse`, Ethernet/IPv4/UDP or raw IP).

## Strategy & risk economics

The signal posts at the **midpoint** (a midpoint peg) and the mock gateway fills it
there, so the strategy captures the mean-reversion edge of the synthetic series
**without paying the spread on every round trip**. PnL therefore stays bounded and
**orders keep flowing throughout a run** — the Daily Loss Limit kill-switch trips
only on a genuine drawdown, not as a structural certainty. (An earlier taker
variant crossed the spread and bled PnL until the kill-switch latched and halted
all order flow after a few seconds; the midpoint peg fixes that.)

All three pre-trade checks stay active and are covered by tests. To see a check
fire on demand:

- `--pos-limit 0` rejects every order (projected position exceeds the cap),
- a low `--loss-limit` trips the daily-loss kill-switch once PnL draws down,
- the price band rejects any order priced far from the market (fat-finger).

Tune `--z-entry/--z-exit/--sigma/--theta` to explore different signal regimes.

## Module map

```
src/mdp3/      CME MDP 3.0 / SBE codec (decode + encode, single source of truth)
src/orderbook  per-instrument depth book + BBO
src/signal     EWMA z-score mean-reversion
src/risk       3 pre-trade checks + position/PnL accounting
src/gateway    mock order gateway + fills
src/timing     portable quanta clock + measured resolution floor
src/metrics    HdrHistograms + reporter + built-in dashboard (assets/dashboard.html)
src/loadgen    OU synthetic MDP3 generator + pcap replay reader
src/pipeline   the inline hot path (decode→book→signal→risk→gateway)
src/runner     threaded orchestration, --calibrate, final report
src/main.rs    CLI
```

## Build profile

Release uses `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, and
`target-cpu=native` (`.cargo/config.toml`). Requires Xcode Command Line Tools for
the C toolchain only if you enable the optional `mimalloc` global allocator.
