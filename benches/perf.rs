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
fn baseline_bytes(states: &[TaskState]) -> usize {
    states
        .iter()
        .map(|t| serde_json::to_string(t).expect("ser").len() + 1)
        .sum()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

/// Median wall time of folding `log` over `baseline`. Pre-clones the (consumed)
/// inputs each iteration so only the materialize itself is timed.
fn bench_materialize(baseline: &[TaskState], log: &[MutationEvent]) -> Duration {
    let times = (0..ITERS)
        .map(|_| (baseline.to_vec(), log.to_vec()))
        .map(|(b, l)| {
            let start = Instant::now();
            let _ = Engine::materialize_state(b, l, STATUS_FIELD, DONE_STATUS);
            start.elapsed()
        })
        .collect();
    median(times)
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

/// Count events by kind: (creates, updates, dependency-adds). `Update`/`Append`/
/// `RemoveDep`/`Delete` all fold into "updates" — this generator only emits the
/// first three kinds anyway.
fn op_mix(log: &[MutationEvent]) -> (usize, usize, usize) {
    let mut creates = 0;
    let mut deps = 0;
    for e in log {
        match e.op {
            OpType::Create => creates += 1,
            OpType::AddDep => deps += 1,
            _ => {}
        }
    }
    (creates, log.len() - creates - deps, deps)
}

/// `part` as a whole-percent of `whole`, rounded.
fn pct(part: usize, whole: usize) -> u64 {
    ((part as f64 / whole as f64) * 100.0).round() as u64
}

fn write_log(path: &Path, log: &[MutationEvent]) {
    let body: String = log
        .iter()
        .map(|e| serde_json::to_string(e).expect("ser") + "\n")
        .collect();
    fs::write(path, body).expect("write");
}

fn bench_replay() {
    // The `dep_pct` knob is the chance a *non-create* event is an `AddDep`; the
    // "create / update / dep" column reports the resulting whole-log mix (creates
    // are ~¼ of every log), so the composition is explicit rather than implied.
    println!("Replay / materialize — by log size and event mix:\n");
    println!("| events | create / update / dep | log size | replay |");
    println!("|---|---|---|---|");
    for n in [1_000usize, 10_000, 100_000, 200_000, 500_000] {
        for dep_pct in [5usize, 20, 50] {
            let log = gen_log(n, dep_pct);
            let (c, u, d) = op_mix(&log);
            let t = bench_materialize(&[], &log);
            println!(
                "| {} | {}% / {}% / {}% | {} | {} |",
                commas(n),
                pct(c, n),
                pct(u, n),
                pct(d, n),
                fmt_bytes(log_bytes(&log)),
                fmt_dur(t),
            );
        }
    }
}

fn bench_compaction() {
    // Each `history` value below appears on TWO rows — the SAME set of events in
    // two storage shapes: the full log, vs a compacted store (a baseline of the
    // folded events plus the retained `keep_events` tail). Both materialize to
    // identical state; we compare on-disk size and the everyday replay time, which
    // is compaction's real payoff. The `dep` field is touched per task, so the mix
    // is the same as the 20%-density rows above (~25% / 60% / 15%).
    let dep_pct = 20usize;
    let (c, u, d) = op_mix(&gen_log(10_000, dep_pct));
    println!(
        "\nCompaction — the SAME history replays from a baseline + retained tail,\n\
         not the full log (keep_events={}, mix {}% / {}% / {}%):\n",
        commas(KEEP_EVENTS),
        pct(c, 10_000),
        pct(u, 10_000),
        pct(d, 10_000),
    );
    println!("| history | stored as | on disk | replay |");
    println!("|---|---|---|---|");
    for n in [100_000usize, 200_000, 500_000] {
        let log = gen_log(n, dep_pct);
        // Measure the uncompacted replay first, in a clean heap — before the
        // baseline is resident — so it agrees with the replay table above.
        let cold = bench_materialize(&[], &log);

        let now = Utc
            .timestamp_opt(1_700_000_000 + n as i64, 0)
            .single()
            .expect("ts");
        let split = Engine::retention_split(&log, KEEP_EVENTS, 0, now);
        let baseline: Vec<TaskState> =
            Engine::materialize_state(Vec::new(), log[..split].to_vec(), STATUS_FIELD, DONE_STATUS)
                .into_values()
                .collect();
        let recent = &log[split..];
        let warm = bench_materialize(&baseline, recent);

        println!(
            "| {} events | full log | {} | {} |",
            commas(n),
            fmt_bytes(log_bytes(&log)),
            fmt_dur(cold),
        );
        println!(
            "| {} events | {}-task baseline + {} tail | {} | {} |",
            commas(n),
            commas(baseline.len()),
            commas(n - split),
            fmt_bytes(baseline_bytes(&baseline) + log_bytes(recent)),
            fmt_dur(warm),
        );
    }
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
