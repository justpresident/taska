//! Performance harness for `taska` — run with `cargo bench`.
//!
//! Deliberately dependency-free: `harness = false` in Cargo.toml makes this a
//! plain `main()` that times operations with `std::time` and prints a markdown
//! table, rather than pulling in criterion (the crate is dependency-cautious).
//! It measures the three costs the protocol doc cites: replay/materialize over a
//! growing log, compaction (fold the old prefix into a fresh baseline), and a
//! merge of two diverged branches.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use serde_json::{json, Map};

use taska::config::OnConflict;
use taska::engine::Engine;
use taska::merge::execute_git_merge;
use taska::model::{MutationEvent, OpType};

/// `[workflow]` status field / done value (only used to compute `close_time`).
const STATUS_FIELD: &str = "status";
const DONE_STATUS: &str = "closed";
/// `[compaction] keep_events` default, used for the compaction split.
const KEEP_EVENTS: usize = 5_000;

/// A synthetic log of `n` events: ~¼ `Create`s, the rest `Update`s with the odd
/// `AddDep`, spread across the created tasks. Seqs are `1..=n`, timestamps 1s
/// apart — a realistic shape for replay/merge cost without any randomness.
fn gen_log(n: usize) -> Vec<MutationEvent> {
    let tasks = (n / 4).max(1);
    let base = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("valid base ts");
    (0..n)
        .map(|i| {
            let task_id = format!("t{}", i % tasks);
            let mut payload = Map::new();
            let op = if i < tasks {
                payload.insert("status".into(), json!("open"));
                payload.insert("title".into(), json!(format!("Task {i}")));
                OpType::Create
            } else if i % 7 == 0 {
                payload.insert("dep".into(), json!(format!("t{}", (i + 1) % tasks)));
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

/// Median of a set of timed runs.
fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

/// Microseconds for sub-millisecond, else milliseconds.
fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else {
        format!("{:.1} ms", us / 1000.0)
    }
}

/// `1234567` -> `1,234,567`.
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
        .map(|e| serde_json::to_string(e).expect("serialize event") + "\n")
        .collect();
    fs::write(path, body).expect("write log");
}

const ITERS: usize = 5;

fn bench_replay() {
    for n in [1_000usize, 10_000, 100_000] {
        let log = gen_log(n);
        // Pre-clone the inputs (materialize consumes its Vec) so only the
        // materialize itself is timed, not the clone.
        let times = (0..ITERS)
            .map(|_| log.clone())
            .map(|input| {
                let start = Instant::now();
                let _ = Engine::materialize_state(Vec::new(), input, STATUS_FIELD, DONE_STATUS);
                start.elapsed()
            })
            .collect();
        println!(
            "| replay / materialize | {} events | {} |",
            commas(n),
            fmt_dur(median(times))
        );
    }
}

fn bench_compaction() {
    for n in [10_000usize, 100_000] {
        let log = gen_log(n);
        let now = Utc
            .timestamp_opt(1_700_000_000 + n as i64, 0)
            .single()
            .expect("valid ts");
        let split = Engine::retention_split(&log, KEEP_EVENTS, 0, now);
        let folded = &log[..split];
        let times = (0..ITERS)
            .map(|_| folded.to_vec())
            .map(|input| {
                let start = Instant::now();
                // Compaction = decide the split, then rebuild the baseline.
                let _ = Engine::retention_split(&log, KEEP_EVENTS, 0, now);
                let _ = Engine::materialize_state(Vec::new(), input, STATUS_FIELD, DONE_STATUS);
                start.elapsed()
            })
            .collect();
        println!(
            "| compaction (fold {} → baseline) | {} events | {} |",
            commas(split),
            commas(n),
            fmt_dur(median(times))
        );
    }
}

fn bench_merge() {
    let anc_n = 1_000usize;
    let m = 100usize; // concurrent events per branch (all conflicting on `owner`)
    let tasks = (anc_n / 4).max(1);
    let anc = gen_log(anc_n);
    let base = Utc
        .timestamp_opt(1_700_000_000 + anc_n as i64, 0)
        .single()
        .expect("valid ts");
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
    fs::create_dir_all(&dir).expect("temp dir");
    let (anc_p, ours_p, theirs_p) = (dir.join("anc"), dir.join("ours"), dir.join("theirs"));
    write_log(&anc_p, &anc);
    write_log(&theirs_p, &theirs);

    // The driver overwrites `ours` with the result, so restore it (untimed) before
    // each timed merge.
    let (a, o, t) = (
        anc_p.to_str().expect("path"),
        ours_p.to_str().expect("path"),
        theirs_p.to_str().expect("path"),
    );
    let mut times: Vec<Duration> = Vec::new();
    for _ in 0..20 {
        write_log(&ours_p, &ours);
        let start = Instant::now();
        execute_git_merge(a, o, t, OnConflict::Ours, None).expect("merge");
        times.push(start.elapsed());
    }
    let _ = fs::remove_dir_all(&dir);
    println!(
        "| merge ({} ancestor + {}/branch concurrent, {} conflicts) | {} events | {} |",
        commas(anc_n),
        m,
        m,
        commas(anc_n + m),
        fmt_dur(median(times))
    );
}

fn main() {
    println!("| operation | size | median |");
    println!("|---|---|---|");
    bench_replay();
    bench_compaction();
    bench_merge();
}
