//! Performance harness for `taska` — run with `cargo bench`.
//!
//! Deliberately dependency-free: `harness = false` in Cargo.toml makes this a
//! plain `main()` that times operations with `std::time` and prints markdown
//! tables, rather than pulling in criterion (the crate is dependency-cautious).
//! It measures replay/materialize across log size *and* dependency density,
//! reports on-disk log sizes, shows what compaction does to size, and times a
//! merge of two diverged branches.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use serde_json::{json, Map};

use taska::config::OnConflict;
use taska::engine::Engine;
use taska::merge::execute_git_merge;
use taska::model::{MutationEvent, OpType, TaskState};

const STATUS_FIELD: &str = "status";
const DONE_STATUS: &str = "closed";
const KEEP_EVENTS: usize = 5_000;
const ITERS: usize = 5;

/// SplitMix64 — a tiny deterministic PRNG, so "random" dependency targets are
/// reproducible run to run without a `rand` dependency.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// A synthetic log of `n` events at a given dependency density. ~¼ of the events
/// are `Create`s; of the rest, `dep_pct`% are `AddDep` to a random task and the
/// remainder are `Update`s. Seqs are `1..=n`, timestamps 1s apart; the PRNG seed
/// is fixed per `(n, dep_pct)` so the log is identical across runs.
fn gen_log(n: usize, dep_pct: usize) -> Vec<MutationEvent> {
    let tasks = (n / 4).max(1);
    let base = Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts");
    let mut rng = Rng(0x5EED ^ (dep_pct as u64) ^ ((n as u64) << 16));
    (0..n)
        .map(|i| {
            let task_id = format!("t{}", i % tasks);
            let mut payload = Map::new();
            let op = if i < tasks {
                payload.insert("status".into(), json!("open"));
                payload.insert("title".into(), json!(format!("Task {i}")));
                OpType::Create
            } else if rng.below(100) < dep_pct {
                payload.insert("dep".into(), json!(format!("t{}", rng.below(tasks))));
                OpType::AddDep
            } else {
                payload.insert("priority".into(), json!(i % 5));
                OpType::Update
            };
            MutationEvent {
                seq: (i + 1) as u64,
                timestamp: base + ChronoDuration::seconds(i as i64),
                op,
                task_id,
                meta: None,
                payload,
            }
        })
        .collect()
}

/// On-disk size of a log as JSONL (one event per line).
fn log_bytes(log: &[MutationEvent]) -> usize {
    log.iter()
        .map(|e| serde_json::to_string(e).expect("ser").len() + 1)
        .sum()
}

/// On-disk size of a materialized baseline as JSONL (one task per line).
fn baseline_bytes(state: &std::collections::HashMap<String, TaskState>) -> usize {
    state
        .values()
        .map(|t| serde_json::to_string(t).expect("ser").len() + 1)
        .sum()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else {
        format!("{:.1} ms", us / 1000.0)
    }
}

fn fmt_bytes(b: usize) -> String {
    let kb = b as f64 / 1024.0;
    if kb < 1024.0 {
        format!("{kb:.0} KB")
    } else {
        format!("{:.1} MB", kb / 1024.0)
    }
}

fn commas(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

fn write_log(path: &Path, log: &[MutationEvent]) {
    let body: String = log
        .iter()
        .map(|e| serde_json::to_string(e).expect("ser") + "\n")
        .collect();
    fs::write(path, body).expect("write");
}

fn bench_replay() {
    println!("Replay / materialize — by log size and random-dependency density:\n");
    println!("| density | events | log size | replay |");
    println!("|---|---|---|---|");
    for dep_pct in [5usize, 20, 50] {
        for n in [1_000usize, 10_000, 100_000] {
            let log = gen_log(n, dep_pct);
            let size = log_bytes(&log);
            // Pre-clone inputs (materialize consumes its Vec) so only the
            // materialize itself is timed.
            let times = (0..ITERS)
                .map(|_| log.clone())
                .map(|input| {
                    let start = Instant::now();
                    let _ = Engine::materialize_state(Vec::new(), input, STATUS_FIELD, DONE_STATUS);
                    start.elapsed()
                })
                .collect();
            println!(
                "| {dep_pct}% | {} | {} | {} |",
                commas(n),
                fmt_bytes(size),
                fmt_dur(median(times))
            );
        }
    }
}

fn bench_compaction() {
    // Compaction's *time* is just a replay of the folded prefix; its point is
    // SIZE — folding old events into a compact baseline bounds on-disk growth.
    let (n, dep_pct) = (100_000usize, 20usize);
    let log = gen_log(n, dep_pct);
    let now = Utc
        .timestamp_opt(1_700_000_000 + n as i64, 0)
        .single()
        .expect("ts");
    let split = Engine::retention_split(&log, KEEP_EVENTS, 0, now);

    let full = log_bytes(&log);
    let baseline =
        Engine::materialize_state(Vec::new(), log[..split].to_vec(), STATUS_FIELD, DONE_STATUS);
    let after = baseline_bytes(&baseline) + log_bytes(&log[split..]);

    let folded = &log[..split];
    let times = (0..ITERS)
        .map(|_| folded.to_vec())
        .map(|input| {
            let start = Instant::now();
            let _ = Engine::materialize_state(Vec::new(), input, STATUS_FIELD, DONE_STATUS);
            start.elapsed()
        })
        .collect();

    println!(
        "\nCompaction — fold the old prefix into a baseline (keep_events={}, {dep_pct}% deps):\n",
        commas(KEEP_EVENTS)
    );
    println!("| from | to | shrink | fold time |");
    println!("|---|---|---|---|");
    println!(
        "| {}-event log ({}) | {} baseline + {} log ({}) | {:.1}× | {} |",
        commas(n),
        fmt_bytes(full),
        commas(baseline.len()),
        commas(n - split),
        fmt_bytes(after),
        full as f64 / after as f64,
        fmt_dur(median(times))
    );
}

fn bench_merge() {
    let anc_n = 1_000usize;
    let m = 100usize; // concurrent events per branch (all conflicting on `owner`)
    let tasks = (anc_n / 4).max(1);
    let anc = gen_log(anc_n, 20);
    let base = Utc
        .timestamp_opt(1_700_000_000 + anc_n as i64, 0)
        .single()
        .expect("ts");
    let branch = |owner: &str| -> Vec<MutationEvent> {
        let mut log = anc.clone();
        log.extend((0..m).map(|i| {
            let mut payload = Map::new();
            payload.insert("owner".into(), json!(owner));
            MutationEvent {
                seq: (anc_n + 1 + i) as u64,
                timestamp: base + ChronoDuration::seconds(i as i64),
                op: OpType::Update,
                task_id: format!("t{}", i % tasks),
                meta: None,
                payload,
            }
        }));
        log
    };
    let ours = branch("alice");
    let theirs = branch("bob");

    let dir = std::env::temp_dir().join(format!("taska-bench-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("dir");
    let (anc_p, ours_p, theirs_p) = (dir.join("anc"), dir.join("ours"), dir.join("theirs"));
    write_log(&anc_p, &anc);
    write_log(&theirs_p, &theirs);

    let (a, o, t) = (
        anc_p.to_str().expect("p"),
        ours_p.to_str().expect("p"),
        theirs_p.to_str().expect("p"),
    );
    let mut times: Vec<Duration> = Vec::new();
    for _ in 0..20 {
        write_log(&ours_p, &ours); // driver overwrites `ours`; restore (untimed)
        let start = Instant::now();
        execute_git_merge(a, o, t, OnConflict::Ours, None).expect("merge");
        times.push(start.elapsed());
    }
    let _ = fs::remove_dir_all(&dir);

    println!("\nMerge — two branches diverged from a shared ancestor:\n");
    println!("| scenario | events | branch log | merge |");
    println!("|---|---|---|---|");
    println!(
        "| {} ancestor + {}/branch concurrent ({} conflicts) | {} | {} | {} |",
        commas(anc_n),
        m,
        m,
        commas(anc_n + m),
        fmt_bytes(log_bytes(&ours)),
        fmt_dur(median(times))
    );
}

fn main() {
    bench_replay();
    bench_compaction();
    bench_merge();
}
