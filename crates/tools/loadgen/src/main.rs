//! Aether load generator and benchmark.
//!
//! ```text
//! aether-loadgen run   --rpc http://127.0.0.1:8545,http://127.0.0.1:8546 \
//!                      --mode transfer|contract --rate 500 --duration 30 --out run.json
//! aether-loadgen bls   --count 1024        # BLS verification: single vs batch vs aggregate
//! ```
//!
//! `run` signs transactions from the deterministic devnet accounts funded by
//! `genesis-ceremony`, submits them at a fixed offered rate in batches
//! (`aeth_sendRawTransactions`) spread over all RPC endpoints, and records the
//! submission time of every transaction. A watcher reads finalized blocks
//! from the first endpoint (`aeth_getBlockSummaries`), which carry the
//! node's own wall-clock finalization time. Finality latency is
//! `finalizedAtMs - submittedAtMs` per transaction (same machine clock).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_crypto_primitives::Keypair;
use aether_sdk::chain;
use aether_types::{Address, Transaction};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Minimal keep-alive JSON-RPC client (one TCP connection per client).
// ---------------------------------------------------------------------------

struct RpcConn {
    host: String,
    port: u16,
    stream: Option<BufReader<TcpStream>>,
}

fn parse_endpoint(url: &str) -> Result<(String, u16)> {
    let rest = url
        .trim()
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("only http:// endpoints are supported: {url}"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    match host_port.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().context("bad port")?)),
        None => Ok((host_port.to_string(), 80)),
    }
}

impl RpcConn {
    fn new(url: &str) -> Result<Self> {
        let (host, port) = parse_endpoint(url)?;
        Ok(Self {
            host,
            port,
            stream: None,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1}).to_string();
        for attempt in 0..2 {
            if self.stream.is_none() {
                let s = TcpStream::connect((self.host.as_str(), self.port)).await?;
                s.set_nodelay(true)?;
                self.stream = Some(BufReader::new(s));
            }
            match self.round_trip(&body).await {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 => {
                    self.stream = None;
                    tracing_note(&format!("rpc reconnect after: {e}"));
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn round_trip(&mut self, body: &str) -> Result<Value> {
        let stream = self.stream.as_mut().expect("connected");
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            self.host,
            self.port,
            body.len(),
            body
        );
        stream.get_mut().write_all(req.as_bytes()).await?;
        let mut status = String::new();
        stream.read_line(&mut status).await?;
        if status.is_empty() {
            bail!("connection closed");
        }
        let mut content_length = None;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let lower = l.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = Some(v.trim().parse::<usize>()?);
            }
            if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            }
        }
        let mut buf = Vec::new();
        if let Some(n) = content_length {
            buf.resize(n, 0);
            stream.read_exact(&mut buf).await?;
        } else if chunked {
            loop {
                let mut size_line = String::new();
                stream.read_line(&mut size_line).await?;
                let size = usize::from_str_radix(size_line.trim(), 16)?;
                let mut chunk = vec![0u8; size + 2];
                stream.read_exact(&mut chunk).await?;
                if size == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..size]);
            }
        } else {
            bail!("response without length");
        }
        if !status.contains(" 200 ") {
            bail!("http status {}", status.trim());
        }
        let v: Value = serde_json::from_slice(&buf)?;
        if let Some(err) = v.get("error") {
            if !err.is_null() {
                bail!("rpc error: {err}");
            }
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn tracing_note(msg: &str) {
    if std::env::var("LOADGEN_DEBUG").is_ok() {
        eprintln!("{msg}");
    }
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RunArgs {
    rpcs: Vec<String>,
    mode: Mode,
    rate: u64,
    duration_s: u64,
    warmup_s: u64,
    drain_s: u64,
    senders: u32,
    first_dev: u32,
    out: Option<String>,
    contract_wat: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Transfer,
    Contract,
}

fn parse_flags(args: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(k) = args[i].strip_prefix("--") {
            let v = args.get(i + 1).cloned().unwrap_or_default();
            map.insert(k.to_string(), v);
            i += 2;
        } else {
            i += 1;
        }
    }
    map
}

impl RunArgs {
    fn parse(args: &[String]) -> Result<Self> {
        let f = parse_flags(args);
        let get = |k: &str, d: &str| f.get(k).cloned().unwrap_or_else(|| d.to_string());
        let rate: u64 = get("rate", "200").parse()?;
        // The mempool admits at most 100 tx/s per sender; stay well below.
        // The mempool admits at most 100 tx/s per sender (anti-spam); use
        // enough senders to stay at <= 20 tx/s each.
        let min_senders = rate.div_ceil(20).max(4) as u32;
        let senders: u32 = get("senders", "0").parse::<u32>()?.max(min_senders);
        if senders > 255 {
            bail!("rate {rate} needs {senders} senders; devnet genesis funds 256 accounts (one is reserved for deploy)");
        }
        Ok(Self {
            rpcs: get("rpc", "http://127.0.0.1:8545")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            mode: match get("mode", "transfer").as_str() {
                "contract" => Mode::Contract,
                "transfer" => Mode::Transfer,
                m => bail!("unknown mode {m}"),
            },
            rate,
            duration_s: get("duration", "30").parse()?,
            warmup_s: get("warmup", "5").parse()?,
            drain_s: get("drain", "30").parse()?,
            senders,
            first_dev: get("first-dev", "0").parse()?,
            out: f.get("out").cloned(),
            contract_wat: get("contract", "examples/contracts/counter.wat"),
        })
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    /// tx hash (hex) -> submitted at (ms)
    submitted: HashMap<String, u64>,
    accepted: u64,
    rejected: u64,
    reject_samples: Vec<String>,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn deploy_counter(rpc: &str, wat_path: &str, deployer: &Keypair) -> Result<Address> {
    let client = aether_sdk::AetherClient::new(rpc.to_string());
    let src = std::fs::read(wat_path).with_context(|| format!("read {wat_path}"))?;
    let code = wat::parse_bytes(&src)
        .map_err(|e| anyhow!("{e}"))?
        .into_owned();
    let chain_id = client.chain_id().await.map_err(|e| anyhow!("{e}"))?;
    let nonce = client
        .next_nonce(deployer)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let (tx, address) = chain::deploy_tx(deployer, chain_id, nonce, code);
    let hash = client
        .send_transaction(&tx)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let status = client
        .wait_for_finality(hash, Duration::from_secs(60))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if status.get("success") != Some(&json!(true)) {
        bail!("deploy failed: {status}");
    }
    Ok(address)
}

async fn run(args: RunArgs) -> Result<Value> {
    let mut probe = RpcConn::new(&args.rpcs[0])?;
    let chain_id_hex = probe.call("aeth_chainId", json!([])).await?;
    let chain_id = u64::from_str_radix(
        chain_id_hex
            .as_str()
            .unwrap_or("0x0")
            .trim_start_matches("0x"),
        16,
    )?;

    let senders: Vec<Keypair> = (0..args.senders)
        .map(|i| Keypair::devnet(args.first_dev + i))
        .collect();
    let contract = if args.mode == Mode::Contract {
        let deployer = Keypair::devnet(255);
        let addr = deploy_counter(&args.rpcs[0], &args.contract_wat, &deployer).await?;
        eprintln!("deployed counter contract at {}", hex0x(addr.as_bytes()));
        Some(addr)
    } else {
        None
    };
    let mut nonces = Vec::with_capacity(senders.len());
    for kp in &senders {
        let addr = chain::address_of(kp);
        let acct = probe
            .call("aeth_getPendingAccount", json!([hex0x(addr.as_bytes())]))
            .await?;
        nonces.push(acct.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0));
    }
    let start_slot = probe
        .call("aeth_getNodeInfo", json!([]))
        .await?
        .get("currentSlot")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let shared = Arc::new(Mutex::new(Shared::default()));
    let recipient = Address::from([0x42u8; 20]);

    // One submitter task per endpoint, fed with signed batches.
    let mut txs_senders = Vec::new();
    let mut handles = Vec::new();
    for url in &args.rpcs {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Transaction>>(256);
        txs_senders.push(tx);
        let shared = shared.clone();
        let mut conn = RpcConn::new(url)?;
        handles.push(tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                let hexes: Vec<String> = batch
                    .iter()
                    .map(|t| hex0x(&bincode::serialize(t).expect("tx serializes")))
                    .collect();
                let hashes: Vec<String> =
                    batch.iter().map(|t| hex0x(t.hash().as_bytes())).collect();
                let sent_at = now_ms();
                let result = conn.call("aeth_sendRawTransactions", json!([hexes])).await;
                let mut s = shared.lock().expect("lock");
                match result {
                    Ok(Value::Array(items)) => {
                        for (h, item) in hashes.into_iter().zip(items) {
                            if item.is_string() {
                                s.accepted += 1;
                                s.submitted.insert(h, sent_at);
                            } else {
                                s.rejected += 1;
                                if s.reject_samples.len() < 10 {
                                    s.reject_samples.push(item.to_string());
                                }
                            }
                        }
                    }
                    other => {
                        s.rejected += hashes.len() as u64;
                        if s.reject_samples.len() < 10 {
                            s.reject_samples.push(format!("{other:?}"));
                        }
                    }
                }
            }
        }));
    }

    // Generate at a fixed offered rate in 20 ms ticks.
    let tick = Duration::from_millis(20);
    let start = Instant::now();
    let start_ms = now_ms();
    let deadline = start + Duration::from_secs(args.duration_s);
    let mut generated: u64 = 0;
    let mut sender_idx = 0usize;
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    // Each sender is pinned to one endpoint so its nonces arrive in order
    // (spreading one sender over several nodes makes later nonces arrive
    // before earlier ones and fill the per-sender queue).
    let n_endpoints = txs_senders.len();
    let mut batches: Vec<Vec<Transaction>> = vec![Vec::new(); n_endpoints];
    while Instant::now() < deadline {
        interval.tick().await;
        let target = (start.elapsed().as_secs_f64() * args.rate as f64) as u64;
        while generated < target {
            let i = sender_idx % senders.len();
            sender_idx += 1;
            let kp = &senders[i];
            let tx = match contract {
                Some(addr) => {
                    chain::call_tx(kp, chain_id, nonces[i], addr, vec![], chain::CALL_GAS)
                }
                None => chain::transfer_tx(kp, chain_id, nonces[i], recipient, 1),
            };
            nonces[i] += 1;
            let e = i % n_endpoints;
            batches[e].push(tx);
            generated += 1;
            if batches[e].len() >= 200 {
                let b = std::mem::take(&mut batches[e]);
                let _ = txs_senders[e].send(b).await;
            }
        }
        for (e, batch) in batches.iter_mut().enumerate() {
            if !batch.is_empty() {
                let b = std::mem::take(batch);
                let _ = txs_senders[e].send(b).await;
            }
        }
    }
    let end_ms = now_ms();
    drop(txs_senders);
    for h in handles {
        let _ = h.await;
    }
    eprintln!(
        "submitted {generated} txs in {:.1}s; waiting up to {}s for finality",
        start.elapsed().as_secs_f64(),
        args.drain_s
    );

    // Collect finalized blocks until all accepted txs are final or drain ends.
    let mut finalized_at: HashMap<String, (u64, u64)> = HashMap::new(); // hash -> (final ms, slot)
    let mut blocks: Vec<Value> = Vec::new();
    let mut scanned_to = start_slot.saturating_sub(1);
    let drain_deadline = Instant::now() + Duration::from_secs(args.drain_s);
    loop {
        let info = probe.call("aeth_getNodeInfo", json!([])).await?;
        let fin = info
            .get("finalizedSlot")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        while scanned_to < fin {
            let to = fin.min(scanned_to + 500);
            let list = probe
                .call("aeth_getBlockSummaries", json!([scanned_to + 1, to, true]))
                .await?;
            for b in list.as_array().cloned().unwrap_or_default() {
                if b.get("finalized") != Some(&json!(true)) {
                    continue;
                }
                let f_ms = b.get("finalizedAtMs").and_then(|v| v.as_u64()).unwrap_or(0);
                let slot = b.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
                for h in b
                    .get("txHashes")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    if let Some(h) = h.as_str() {
                        finalized_at.insert(h.to_string(), (f_ms, slot));
                    }
                }
                let mut summary = b.clone();
                if let Some(o) = summary.as_object_mut() {
                    o.remove("txHashes");
                }
                blocks.push(summary);
            }
            scanned_to = to;
        }
        let (pending, total) = {
            let s = shared.lock().expect("lock");
            let done = s
                .submitted
                .keys()
                .filter(|h| finalized_at.contains_key(*h))
                .count();
            (s.submitted.len() - done, s.submitted.len())
        };
        if (pending == 0 && total > 0) || Instant::now() > drain_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let s = shared.lock().expect("lock");
    let window_start = start_ms + args.warmup_s * 1000;
    let window_end = end_ms;
    let mut latencies: Vec<u64> = Vec::new();
    let mut finalized_in_window = 0u64;
    let mut finalized_total = 0u64;
    let mut failed_receipts = 0u64;
    for (h, sent) in &s.submitted {
        if let Some((f, _)) = finalized_at.get(h) {
            finalized_total += 1;
            if *f >= window_start && *f <= window_end {
                finalized_in_window += 1;
            }
            if *sent >= window_start {
                latencies.push(f.saturating_sub(*sent));
            }
        }
    }
    let _ = &mut failed_receipts;
    latencies.sort_unstable();
    let window_s = (window_end.saturating_sub(window_start)) as f64 / 1000.0;
    let run_blocks: Vec<&Value> = blocks
        .iter()
        .filter(|b| {
            let seen = b.get("seenAtMs").and_then(|v| v.as_u64()).unwrap_or(0);
            seen >= start_ms && seen <= end_ms
        })
        .collect();
    let slots_in_run = (end_ms - start_ms) as f64 / 500.0;
    let mut block_intervals: Vec<u64> = Vec::new();
    let mut prev_slot: Option<u64> = None;
    for b in &run_blocks {
        let slot = b.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(p) = prev_slot {
            block_intervals.push((slot - p) * 500);
        }
        prev_slot = Some(slot);
    }
    block_intervals.sort_unstable();
    let txs_per_block: Vec<u64> = run_blocks
        .iter()
        .map(|b| b.get("txCount").and_then(|v| v.as_u64()).unwrap_or(0))
        .collect();
    let mean_block_interval = if block_intervals.is_empty() {
        0.0
    } else {
        block_intervals.iter().sum::<u64>() as f64 / block_intervals.len() as f64
    };

    let report = json!({
        "mode": if args.mode == Mode::Contract { "contract" } else { "transfer" },
        "offeredRateTps": args.rate,
        "durationS": args.duration_s,
        "warmupS": args.warmup_s,
        "senders": args.senders,
        "endpoints": args.rpcs.len(),
        "generated": generated,
        "accepted": s.accepted,
        "rejectedAtSubmit": s.rejected,
        "rejectSamples": s.reject_samples,
        "finalized": finalized_total,
        "notFinalizedAfterDrain": s.submitted.len() as u64 - finalized_total,
        "finalizedTpsSteadyWindow": if window_s > 0.0 { finalized_in_window as f64 / window_s } else { 0.0 },
        "finalityMs": {
            "samples": latencies.len(),
            "p50": percentile(&latencies, 50.0),
            "p95": percentile(&latencies, 95.0),
            "p99": percentile(&latencies, 99.0),
            "max": latencies.last().copied().unwrap_or(0),
        },
        "blocks": {
            "finalizedBlocksDuringRun": run_blocks.len(),
            "slotsDuringRun": slots_in_run,
            "meanFinalizedBlockIntervalMs": mean_block_interval,
            "p50FinalizedBlockIntervalMs": percentile(&block_intervals, 50.0),
            "maxTxsPerBlock": txs_per_block.iter().max().copied().unwrap_or(0),
            "meanTxsPerBlock": if txs_per_block.is_empty() { 0.0 } else { txs_per_block.iter().sum::<u64>() as f64 / txs_per_block.len() as f64 },
        },
        "contract": contract.map(|a| hex0x(a.as_bytes())),
    });
    Ok(report)
}

// ---------------------------------------------------------------------------
// BLS verification benchmark
// ---------------------------------------------------------------------------

fn bls_bench(count: usize) -> Result<Value> {
    use aether_crypto_bls::{
        aggregate_public_keys, aggregate_signatures, keypair, verify_aggregated, verify_batch,
        BlsKeypair,
    };
    let keys: Vec<BlsKeypair> = (0..count).map(|_| BlsKeypair::generate()).collect();
    let msgs: Vec<Vec<u8>> = (0..count)
        .map(|i| format!("vote-{i}").into_bytes())
        .collect();
    let sigs: Vec<Vec<u8>> = keys.iter().zip(&msgs).map(|(k, m)| k.sign(m)).collect();
    let pks: Vec<Vec<u8>> = keys.iter().map(|k| k.public_key()).collect();

    let t = Instant::now();
    for i in 0..count {
        if !keypair::verify(&pks[i], &msgs[i], &sigs[i])? {
            bail!("single verify failed");
        }
    }
    let single = t.elapsed();

    let tuples: Vec<(&[u8], &[u8], &[u8])> = (0..count)
        .map(|i| (pks[i].as_slice(), msgs[i].as_slice(), sigs[i].as_slice()))
        .collect();
    let t = Instant::now();
    if !verify_batch(&tuples)? {
        bail!("batch verify failed");
    }
    let batch = t.elapsed();

    // Quorum certificate case: everyone signs the SAME message (block hash ||
    // slot); verify one aggregate signature against the aggregate key.
    let same_msg = b"block-hash||slot".to_vec();
    let qc_sigs: Vec<Vec<u8>> = keys.iter().map(|k| k.sign(&same_msg)).collect();
    let t = Instant::now();
    let agg_sig = aggregate_signatures(&qc_sigs)?;
    let agg_pk = aggregate_public_keys(&pks)?;
    if !verify_aggregated(&agg_pk, &same_msg, &agg_sig)? {
        bail!("aggregate verify failed");
    }
    let aggregate = t.elapsed();

    let per_sec = |d: Duration| count as f64 / d.as_secs_f64();
    Ok(json!({
        "signatures": count,
        "singleVerify": { "totalMs": single.as_secs_f64() * 1e3, "sigsPerSec": per_sec(single) },
        "batchVerifyDistinctMessages": { "totalMs": batch.as_secs_f64() * 1e3, "sigsPerSec": per_sec(batch) },
        "aggregateSameMessage": { "totalMs": aggregate.as_secs_f64() * 1e3, "sigsPerSec": per_sec(aggregate), "note": "aggregate pubkeys + signatures, then one pairing check (QC verification)" },
        "batchSpeedup": single.as_secs_f64() / batch.as_secs_f64(),
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = argv.get(1).map(String::as_str).unwrap_or("help");
    let rest = if argv.len() > 2 { &argv[2..] } else { &[] };
    let report = match cmd {
        "run" => {
            let args = RunArgs::parse(rest)?;
            let out = args.out.clone();
            let report = run(args).await?;
            if let Some(path) = out {
                std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
            }
            report
        }
        "bls" => {
            let f = parse_flags(rest);
            let count: usize = f
                .get("count")
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(1024);
            let mut runs = Vec::new();
            for _ in 0..3 {
                runs.push(bls_bench(count)?);
            }
            json!({ "runs": runs })
        }
        _ => {
            eprintln!("usage: aether-loadgen run --rpc <url,...> --mode transfer|contract --rate <tps> --duration <s> [--warmup s] [--drain s] [--senders n] [--out file]");
            eprintln!("       aether-loadgen bls [--count n]");
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:8545").unwrap(),
            ("127.0.0.1".into(), 8545)
        );
        assert_eq!(parse_endpoint("http://node/").unwrap(), ("node".into(), 80));
        assert!(parse_endpoint("https://x:1").is_err());
    }

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 51);
        assert_eq!(percentile(&v, 99.0), 99);
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn senders_scale_with_rate() {
        let args: Vec<String> = ["--rate", "1200"].iter().map(|s| s.to_string()).collect();
        let parsed = RunArgs::parse(&args).unwrap();
        assert!(parsed.senders >= 60);
        let too_fast: Vec<String> = ["--rate", "10000"].iter().map(|s| s.to_string()).collect();
        assert!(RunArgs::parse(&too_fast).is_err());
    }

    #[test]
    fn bls_bench_small() {
        let r = bls_bench(8).unwrap();
        assert_eq!(r["signatures"], 8);
    }
}
