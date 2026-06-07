//! Performance harness for `taska` — run with `cargo bench`.
//!
//! Deliberately dependency-free: `harness = false` in Cargo.toml makes this a
//! plain `main()` that times operations with `std::time` and prints markdown
//! tables, rather than pulling in criterion (the crate is dependency-cautious).
//! It measures replay/materialize across log size *and* dependency density,
//! reports on-disk log sizes, shows what compaction does to size, and times a
//! merge of two diverged branches.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use serde_json::{json, Map};
use smallvec::SmallVec;

use taska::config::OnConflict;
use taska::engine::Engine;
use taska::graph;
use taska::merge::execute_git_merge;
use taska::model::{MutationEvent, OpType, TaskState};

/// Typed relationship kinds used to populate the per-task `relationships` map.
const REL_TYPES: [&str; 3] = ["relates_to", "blocks", "duplicates"];

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
/// are `Create`s; of the rest, `dep_pct`% are `AddEdge` to a random task and the
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
                payload.insert("target".into(), json!(format!("t{}", rng.below(tasks))));
                OpType::AddEdge
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
            OpType::AddEdge => deps += 1,
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
    // The `dep_pct` knob is the chance a *non-create* event is an `AddEdge`; the
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

/// Like `gen_log`, but every dependency-add is spread across `depends_on` and
/// the three [`REL_TYPES`], so the per-task `relationships` BTreeMap is heavily
/// populated — the storage this measurement targets. Edges always point to a
/// lower-indexed task, so the graph stays acyclic and the toposort/ready paths
/// run in full rather than bailing on a cycle.
fn gen_rel_log(n: usize) -> Vec<MutationEvent> {
    let tasks = (n / 4).max(1);
    let base = Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts");
    let mut rng = Rng(0x9E37_C0DE ^ (n as u64));
    (0..n)
        .map(|i| {
            let src = i % tasks;
            let task_id = format!("t{src}");
            let mut payload = Map::new();
            let op = if i < tasks {
                payload.insert("status".into(), json!("open"));
                payload.insert("title".into(), json!(format!("Task {i}")));
                OpType::Create
            } else if src == 0 {
                // Task 0 has nothing lower to point at; emit a plain update.
                payload.insert("priority".into(), json!(i % 5));
                OpType::Update
            } else {
                payload.insert("target".into(), json!(format!("t{}", rng.below(src))));
                // 1-in-4 stays a plain depends_on; the rest become typed edges.
                let k = rng.below(REL_TYPES.len() + 1);
                if k < REL_TYPES.len() {
                    payload.insert("rel".into(), json!(REL_TYPES[k]));
                }
                OpType::AddEdge
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

/// Median wall time of `f` over `iters` runs; the result is black-boxed so the
/// optimizer can't elide the work.
fn time_op<R>(iters: usize, mut f: impl FnMut() -> R) -> Duration {
    let times = (0..iters)
        .map(|_| {
            let start = Instant::now();
            let r = f();
            let elapsed = start.elapsed();
            std::hint::black_box(r);
            elapsed
        })
        .collect();
    median(times)
}

/// `graph::blocker_edges` as a lazy iterator — produces the edges with no
/// intermediate collection, so a consumer that only iterates allocates nothing.
fn edge_iter<'a>(
    task: &'a TaskState,
    blockers: &'a BTreeSet<String>,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    // depends_on now lives in the relationships map like every other type.
    task.relationships
        .iter()
        .filter(move |(rel, _)| blockers.contains(rel.as_str()))
        .flat_map(|(rel, targets)| targets.iter().map(move |t| (t.as_str(), rel.as_str())))
}

/// The same edges materialized into an inline-buffer SmallVec (≤4 edges stay on
/// the stack), so a consumer can sort/index in place without a heap allocation.
fn edge_smallvec<'a>(
    task: &'a TaskState,
    blockers: &'a BTreeSet<String>,
) -> SmallVec<[(&'a str, &'a str); 4]> {
    edge_iter(task, blockers).collect()
}

fn bench_relationships() {
    let blockers: BTreeSet<String> = ["depends_on", "blocks", "duplicates"]
        .into_iter()
        .map(String::from)
        .collect();

    // Resident heap of the per-task relationship storage at 100k events.
    let state =
        Engine::materialize_state(Vec::new(), gen_rel_log(100_000), STATUS_FIELD, DONE_STATUS);
    let word = std::mem::size_of::<String>();
    let (mut content, mut typename_bytes) = (0usize, 0usize);
    let (mut maps, mut vecs, mut rel_edges, mut dep_edges) = (0usize, 0usize, 0usize, 0usize);
    for (key, t) in &state {
        content += key.len() + t.id.len();
        if !t.relationships.is_empty() {
            maps += 1;
        }
        // depends_on is now a relationship entry like the rest; split the edge
        // counts for the report but account every entry's heap once.
        for (rel_type, targets) in &t.relationships {
            vecs += 1;
            if rel_type == "depends_on" {
                dep_edges += targets.len();
            } else {
                rel_edges += targets.len();
            }
            typename_bytes += rel_type.len();
            content += rel_type.len()
                + targets.capacity() * word
                + targets.iter().map(String::len).sum::<usize>();
        }
    }

    println!(
        "\nRelationship storage — materialized state of 100,000 events ({} tasks):\n",
        commas(state.len())
    );
    println!("| metric | value |");
    println!("|---|---|");
    println!("| depends_on edges | {} |", commas(dep_edges));
    println!(
        "| typed relationship edges | {} (in {} per-task maps / {} per-type vecs) |",
        commas(rel_edges),
        commas(maps),
        commas(vecs),
    );
    println!(
        "| heap content (ids + dep/rel targets + type names) | {} |",
        fmt_bytes(content),
    );
    println!(
        "| of which duplicated type-name strings | {} ({}%) |",
        fmt_bytes(typename_bytes),
        pct(typename_bytes, content),
    );

    // Per-task blocker-edge access in a materialize-and-sort workload (what
    // `dep tree` does), to settle Vec vs SmallVec where the edges are actually
    // collected — not just iterated. The iterate-only floor shows the work that
    // remains once the allocation is removed entirely.
    let floor = time_op(20, || {
        state
            .values()
            .map(|t| edge_iter(t, &blockers).count())
            .sum::<usize>()
    });
    let vec_sort = time_op(20, || {
        let mut total = 0usize;
        for t in state.values() {
            let mut v: Vec<(&str, &str)> = edge_iter(t, &blockers).collect();
            v.sort_unstable();
            total += v.len();
        }
        total
    });
    let sv_sort = time_op(20, || {
        let mut total = 0usize;
        for t in state.values() {
            let mut v = edge_smallvec(t, &blockers);
            v.sort_unstable();
            total += v.len();
        }
        total
    });

    // Graph traversal. toposort/ready are O(V+E) — cheap even at 25k tasks;
    // reachability is O(V·(V+E)), so it runs over a smaller store.
    let order = time_op(ITERS, || {
        graph::validate_and_sort_dependencies(&state, &blockers)
    });
    let ready = time_op(ITERS, || {
        graph::ready_tasks(&state, STATUS_FIELD, DONE_STATUS, &blockers)
    });
    let small =
        Engine::materialize_state(Vec::new(), gen_rel_log(8_000), STATUS_FIELD, DONE_STATUS);
    let reach = time_op(3, || {
        graph::reachability_counts(&small, &blockers, STATUS_FIELD, DONE_STATUS)
    });

    println!("\nGraph traversal (blocker edges = depends_on + 2 typed kinds):\n");
    println!("| operation | tasks | median |");
    println!("|---|---|---|");
    println!(
        "| edges: iterate only (floor) | {} | {} |",
        commas(state.len()),
        fmt_dur(floor),
    );
    println!(
        "| edges: collect Vec + sort | {} | {} |",
        commas(state.len()),
        fmt_dur(vec_sort),
    );
    println!(
        "| edges: collect SmallVec + sort | {} | {} |",
        commas(state.len()),
        fmt_dur(sv_sort),
    );
    println!(
        "| validate + toposort | {} | {} |",
        commas(state.len()),
        fmt_dur(order),
    );
    println!(
        "| ready_tasks | {} | {} |",
        commas(state.len()),
        fmt_dur(ready)
    );
    println!(
        "| reachability_counts | {} | {} |",
        commas(small.len()),
        fmt_dur(reach),
    );

    bench_dep_type_id(&state, &blockers);
}

/// DepTypeId prototype: would interning relationship-type names to integers help?
/// The hot graph ops discard the edge type; the only per-task type work is the
/// blocker-membership filter inside `blocker_edges`. Compare three ways to do that
/// filter over every task's edges: the current `BTreeSet<String>` membership; a
/// `DepTypeId` that still maps each type *string* → id (because the materialized
/// `relationships` map is string-keyed); and a "ceiling" where edges are already
/// stored as `(DepTypeId, target)` so the filter is integer-only.
fn bench_dep_type_id(state: &HashMap<String, TaskState>, blockers: &BTreeSet<String>) {
    let all_types = ["depends_on", "relates_to", "blocks", "duplicates"];
    let type_to_id: HashMap<&str, u32> = all_types
        .iter()
        .enumerate()
        .map(|(i, &t)| (t, i as u32))
        .collect();
    let blocker_mask: Vec<bool> = all_types.iter().map(|t| blockers.contains(*t)).collect();

    // (1) Current: BTreeSet<String> membership, once per type per task.
    let string_filter = time_op(20, || {
        state
            .values()
            .map(|t| edge_iter(t, blockers).count())
            .sum::<usize>()
    });

    // (2) DepTypeId, realistic: the relationships map is string-keyed, so each type
    // must still be hashed to look up its id before the integer mask check.
    let int_filter = time_op(20, || {
        let mut total = 0usize;
        for t in state.values() {
            for (rel, targets) in &t.relationships {
                if type_to_id
                    .get(rel.as_str())
                    .is_some_and(|&id| blocker_mask[id as usize])
                {
                    total += targets.len();
                }
            }
        }
        total
    });

    // (3) Ceiling: edges pre-interned to (DepTypeId, target) once, as id-keyed
    // in-memory storage would hold them; the filter is then integer-only.
    let interned: Vec<Vec<(u32, &str)>> = state
        .values()
        .map(|t| {
            let mut v: Vec<(u32, &str)> = Vec::new();
            for (rel, targets) in &t.relationships {
                if let Some(&id) = type_to_id.get(rel.as_str()) {
                    v.extend(targets.iter().map(|x| (id, x.as_str())));
                }
            }
            v
        })
        .collect();
    let int_only = time_op(20, || {
        interned
            .iter()
            .flatten()
            .filter(|(id, _)| blocker_mask[*id as usize])
            .count()
    });

    println!("\nDepTypeId — blocker-membership filter over every task's edges:\n");
    println!("| filter | tasks | median |");
    println!("|---|---|---|");
    println!(
        "| BTreeSet<String> (current) | {} | {} |",
        commas(state.len()),
        fmt_dur(string_filter),
    );
    println!(
        "| DepTypeId (string→id lookup + mask) | {} | {} |",
        commas(state.len()),
        fmt_dur(int_filter),
    );
    println!(
        "| pre-interned ids (ceiling, no string work) | {} | {} |",
        commas(state.len()),
        fmt_dur(int_only),
    );
}

fn main() {
    bench_replay();
    bench_compaction();
    bench_merge();
    bench_relationships();
}
