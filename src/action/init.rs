//! `init` action: provision the store, register the git merge drivers, and sync
//! the agent-integration block in `CLAUDE.md` / `AGENTS.md`.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::action::prime;
use crate::error::DynError;
use crate::git;
use crate::storage::FileStore;

/// Whether `init` reused an existing store or created a new one (with its path).
pub enum StoreInit {
    Reused(PathBuf),
    Created(PathBuf),
}

/// What `init` did to one agent-integration file.
pub enum AgentFileStatus {
    /// The file did not exist; `init` created it with the block.
    Created,
    /// The file existed; `init` spliced in a new/changed block.
    Updated,
    /// The file already held exactly this block — left untouched.
    Unchanged,
}

/// One agent file `init` synced, and what happened to it.
pub struct AgentFile {
    pub path: PathBuf,
    pub status: AgentFileStatus,
}

/// The full result of `init`: the store outcome plus every agent file synced.
pub struct InitOutcome {
    pub store: StoreInit,
    pub agent_files: Vec<AgentFile>,
}

/// The integration-block markers.
///
/// The BEGIN line also carries a version and a content hash, so a re-run can
/// splice a fresh block in place and a human/tool can see at a glance whether
/// it's current. Existing files in the wild carry these exact strings — treat
/// them as a format contract; don't rename without a migration story.
const BLOCK_BEGIN: &str = "<!-- BEGIN TASKA INTEGRATION";
const BLOCK_END: &str = "<!-- END TASKA INTEGRATION -->";
/// The block's schema version (bump when the body format changes).
const BLOCK_VERSION: u32 = 2;
/// Candidate agent files. Every one that already exists is updated; if none do,
/// the FIRST is created (`AGENTS.md` — the emerging cross-tool standard).
const AGENT_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Provision the store idempotently, (re)register the git merge driver, and sync
/// the agent-integration block.
///
/// Reuse a discoverable store (so re-running from anywhere in the repo is
/// idempotent), else create one at the SCM root — committed there, the store
/// travels with the repo and every clone's walk-up discovery finds it; only a
/// plain directory (no SCM above) keeps it at the cwd. The driver is always
/// (re)registered, and the integration block is always re-synced, so re-running
/// is how a clone installs both.
pub fn init() -> Result<InitOutcome, DynError> {
    let (base_dir, store_outcome) = if let Ok(existing) = FileStore::discover() {
        let dir = existing.base_dir;
        (dir.clone(), StoreInit::Reused(dir))
    } else {
        let cwd = std::env::current_dir()?;
        let root = git::scm_root(&cwd).map(Path::to_path_buf).unwrap_or(cwd);
        let dir = root.join(".taska");
        (dir.clone(), StoreInit::Created(dir))
    };

    // Provision honors the (possibly user-edited) config, creating any newly
    // configured log files — re-running `init` is how a `[store]` path change is
    // applied.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?
        .to_path_buf();
    git::setup(&repo_root)?;

    // Best-effort: a store whose config can't be read still gets its driver set
    // up (the integration block is a nicety, not load-bearing). The next `ta`
    // command surfaces a real config problem through the normal gate.
    let agent_files = match prime::prime(&store) {
        Ok(outcome) => sync_agent_files(&repo_root, &integration_block(&outcome.facts))?,
        Err(_) => Vec::new(),
    };

    Ok(InitOutcome {
        store: store_outcome,
        agent_files,
    })
}

/// Update every existing agent file with the block; if none exist, create the
/// preferred one (`AGENTS.md`).
fn sync_agent_files(repo_root: &Path, block: &str) -> Result<Vec<AgentFile>, DynError> {
    let existing: Vec<&str> = AGENT_FILES
        .iter()
        .copied()
        .filter(|name| repo_root.join(name).is_file())
        .collect();
    let targets: Vec<&str> = if existing.is_empty() {
        vec![AGENT_FILES[0]]
    } else {
        existing
    };

    let mut out = Vec::new();
    for name in targets {
        let path = repo_root.join(name);
        let existed = path.is_file();
        let current = if existed {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        let status = match splice_block(&current, block) {
            Some(updated) => {
                fs::write(&path, updated)?;
                if existed {
                    AgentFileStatus::Updated
                } else {
                    AgentFileStatus::Created
                }
            }
            None => AgentFileStatus::Unchanged,
        };
        out.push(AgentFile { path, status });
    }
    Ok(out)
}

/// Splice `block` into `existing` between the taska markers (replacing any prior
/// block in place), or append it when absent. Returns `None` when the file
/// already holds exactly this block (so `init` reports it unchanged).
fn splice_block(existing: &str, block: &str) -> Option<String> {
    if let Some(start) = existing.find(BLOCK_BEGIN) {
        if let Some(end_off) = existing[start..].find(BLOCK_END) {
            let end = start + end_off + BLOCK_END.len();
            if existing[start..end] == *block {
                return None;
            }
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(block);
            out.push_str(&existing[end..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return Some(out);
        }
    }
    // No complete marker pair: append after the existing content.
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    Some(out)
}

/// Build the marker-delimited integration block from the config-tailored facts —
/// a condensed, runnable cheat-sheet that points at `ta prime` for the full,
/// always-current guide (kept deliberately small per taska's non-intrusive bent).
fn integration_block(facts: &prime::PrimeFacts) -> String {
    let ex = prime::examples(facts);
    let (sf, tf) = (&facts.status_field, &facts.type_field);
    let cmds: Vec<(String, String)> = vec![
        (
            "ta list --ready".to_string(),
            "pick actionable work".to_string(),
        ),
        (
            "ta show <id> --full".to_string(),
            "full details of one task".to_string(),
        ),
        (
            format!("ta create <id> {tf}={} {}", ex.type_name, ex.req_example),
            "file work — rich notes: goal, approach, open Qs".to_string(),
        ),
        (
            format!("ta update <id> {sf}={}", ex.claim),
            format!("=, +=, -=  ({sf}={} to finish)", facts.done_status),
        ),
        (
            format!("ta dep add <id> {}=<other>", ex.blocker),
            "record a prerequisite".to_string(),
        ),
        (
            "ta update <id> notes+=\"…\"".to_string(),
            "append a note (here and on related tasks)".to_string(),
        ),
    ];
    let width = cmds
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    let cheat = cmds
        .iter()
        .map(|(c, n)| format!("{c:<width$}  # {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let body = format!(
        "## Task tracking (taska)\n\
         \n\
         This repo tracks work in a local, git-native store (`.taska/`) — drive it through \
         the `ta` CLI, never hand-edit `.taska/`. The task schema (fields, statuses, types) \
         is set by `.taska/config.toml` and differs per repo; run `ta prime` for THIS \
         store's schema and the full workflow.\n\
         \n\
         ```bash\n\
         {cheat}\n\
         ```\n\
         \n\
         File a task for each unit of work, with enough `notes` to act on it (goal, approach, \
         open questions); set prerequisites with `ta dep`, and append progress to related \
         tasks. Commit the `.taska/` change in the same commit as the code it describes."
    );
    let hash = short_hash(&body);
    format!("{BLOCK_BEGIN} v{BLOCK_VERSION} hash:{hash} -->\n{body}\n{BLOCK_END}")
}

/// A short, deterministic content hash for the BEGIN marker. `DefaultHasher` is
/// seeded with fixed keys, so the digest is stable across runs (what idempotency
/// needs) without pulling in a crypto dependency.
fn short_hash(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::store_with_schema;

    fn block() -> String {
        integration_block(&prime::prime(&store_with_schema()).unwrap().facts)
    }

    #[test]
    fn block_is_marker_delimited_and_config_tailored() {
        let b = block();
        assert!(b.starts_with(BLOCK_BEGIN), "begins with the marker: {b}");
        assert!(b.contains("hash:"), "carries a content hash: {b}");
        assert!(
            b.trim_end().ends_with(BLOCK_END),
            "ends with the marker: {b}"
        );
        // The cheat-sheet uses the store's actual vocabulary.
        assert!(
            b.contains("ta create <id> type=task"),
            "create example: {b}"
        );
        assert!(
            b.contains("ta update <id> status=in_progress"),
            "claim example: {b}"
        );
        assert!(b.contains("status=closed to finish"), "done status: {b}");
        assert!(b.contains("ta prime"), "points at the full guide: {b}");
        // It teaches the dynamic schema and the task-filing discipline.
        assert!(
            b.contains("schema") && b.contains(".taska/config.toml"),
            "explains the dynamic schema: {b}"
        );
        assert!(
            b.contains("File a task for each unit of work")
                && b.contains("open questions")
                && b.contains("ta dep"),
            "encourages rich tasks + dependencies: {b}"
        );
        assert!(
            b.contains("append progress to related tasks"),
            "encourages cross-task notes: {b}"
        );
    }

    #[test]
    fn splice_appends_then_replaces_in_place_then_no_ops() {
        let doc = "# My project\n\nSome notes.\n";
        let b1 = "<!-- BEGIN TASKA INTEGRATION v1 hash:aaaaaaaa -->\nbody one\n<!-- END TASKA INTEGRATION -->";

        // First sync appends after the existing content.
        let after_append = splice_block(doc, b1).expect("appends");
        assert!(after_append.starts_with("# My project"), "keeps the doc");
        assert!(after_append.contains(b1), "adds the block");

        // Re-syncing the SAME block is a no-op.
        assert!(
            splice_block(&after_append, b1).is_none(),
            "identical block is unchanged"
        );

        // A changed block replaces the old one IN PLACE (not appended again).
        let b2 = "<!-- BEGIN TASKA INTEGRATION v1 hash:bbbbbbbb -->\nbody two\n<!-- END TASKA INTEGRATION -->";
        let after_update = splice_block(&after_append, b2).expect("replaces");
        assert!(after_update.contains("body two"), "new body present");
        assert!(!after_update.contains("body one"), "old body gone");
        assert_eq!(
            after_update.matches(BLOCK_BEGIN).count(),
            1,
            "exactly one block remains: {after_update}"
        );
    }

    #[test]
    fn hash_is_stable_for_the_same_content() {
        assert_eq!(short_hash("hello"), short_hash("hello"));
        assert_ne!(short_hash("hello"), short_hash("world"));
    }
}
