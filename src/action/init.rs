//! `init` action: provision the store, register the git merge drivers, and sync
//! the agent-integration block in `CLAUDE.md` / `AGENTS.md`.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

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
    /// The file already held exactly this block - left untouched.
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
/// it's current. Existing files in the wild carry these exact strings - treat
/// them as a format contract; don't rename without a migration story.
const BLOCK_BEGIN: &str = "<!-- BEGIN TASKA INTEGRATION";
const BLOCK_END: &str = "<!-- END TASKA INTEGRATION -->";
/// The block's schema version (bump when the guidance text changes).
const BLOCK_VERSION: u32 = 5;
/// Candidate agent files. Every one that already exists is updated; if none do,
/// the FIRST is created (`AGENTS.md` - the emerging cross-tool standard).
const AGENT_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Provision the store idempotently, (re)register the git merge driver, and sync
/// the agent-integration block.
///
/// Reuse a discoverable store (so re-running from anywhere in the repo is
/// idempotent), else create one at the SCM root - committed there, the store
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
    // configured log files - re-running `init` is how a `[store]` path change is
    // applied.
    let store = FileStore::provision(base_dir)?;
    let repo_root = store
        .repo_root()
        .ok_or("could not determine repository root from the .taska directory")?
        .to_path_buf();
    git::setup(&repo_root)?;

    // The block is config-AGNOSTIC (durable bare commands + working habits +
    // pointers to `ta prime`/`--help`), so it needs nothing from the store and
    // never drifts as the config changes - `ta prime` carries the dynamic detail.
    let agent_files = sync_agent_files(&repo_root, &integration_block())?;

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

/// Build the marker-delimited integration block.
///
/// Deliberately CONFIG-AGNOSTIC: bare command shapes (placeholders, not this
/// store's field/status/type/relationship names), the durable working habits,
/// and pointers to `ta prime` (for the dynamic, config-tailored schema and
/// examples) and `ta <command> --help` (for flags). Because it names nothing that
/// can change, it never drifts as the config evolves - the same block fits every
/// store, and re-running `init` only rewrites it when this guidance text changes.
fn integration_block() -> String {
    // Bare command shapes - `<field>`/`<type>`/`<target>` placeholders, never a
    // configured name. The aligned comments line up via the longest command.
    let cmds: [(&str, &str); 6] = [
        (
            "ta list --ready",
            "actionable work: not done, all deps done",
        ),
        ("ta show <id> --full", "one task - every field, full notes"),
        (
            "ta create <id> <field>=<value> ...",
            "file new work (the status field defaults - don't set it)",
        ),
        (
            "ta update <id> <field>=<value> ...",
            "=, +=, -=  (set / append / remove)",
        ),
        ("ta dep add <id> <type>=<target>", "link a dependency"),
        ("ta status", "counts"),
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
         This repo tracks work in a local, git-native store (`.taska/`) - drive it through the \
         `ta` CLI, never hand-edit `.taska/` and never `git restore` it out from under in-flight \
         work (either corrupts the append-only log). Field names, statuses, task types, and \
         relationships are defined by `.taska/config.toml` and vary per repo, so run `ta prime` \
         for THIS store's schema and copy-paste-ready examples, and `ta <command> --help` for a \
         command's flags.\n\
         \n\
         ```bash\n\
         {cheat}\n\
         ```\n\
         \n\
         Working habits:\n\
         - File a task for each distinct piece of work, before or as you start it, with `notes` \
         rich enough for someone else to act on: the goal, intended approach/implementation \
         details, and any open or design questions.\n\
         - For long or multi-line values, read from stdin (`<field>=@-`) or a file \
         (`<field>=@FILE`) instead of quoting on the command line (`+=`/`-=` accept `@` too).\n\
         - Set prerequisites with `ta dep add`, and append progress to related tasks \
         (`<field>+=...`) as things change so the trail stays current.\n\
         - Read a task's full, untruncated notes with `ta show <id> --full`.\n\
         - Commit the `.taska/` change in the same commit as the code it describes; if the \
         store has pending changes unrelated to what you're starting, commit those first."
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

    #[test]
    fn block_is_marker_delimited_and_config_agnostic() {
        let b = integration_block();
        assert!(b.starts_with(BLOCK_BEGIN), "begins with the marker: {b}");
        assert!(b.contains("hash:"), "carries a content hash: {b}");
        assert!(
            b.trim_end().ends_with(BLOCK_END),
            "ends with the marker: {b}"
        );
        // Pointers to the dynamic guide and per-command help - not baked-in detail.
        assert!(b.contains("ta prime"), "points at the dynamic guide: {b}");
        assert!(b.contains("--help"), "points at per-command help: {b}");
        // Config-AGNOSTIC: generic placeholders, never a configured field/type/
        // status/relationship literal.
        assert!(
            b.contains("ta create <id> <field>=<value>"),
            "generic create shape: {b}"
        );
        assert!(
            !b.contains("type=task") && !b.contains("depends_on") && !b.contains("=todo"),
            "no config-specific literals: {b}"
        );
        // The durable working habits are present.
        assert!(
            b.contains("File a task for each distinct piece of work")
                && b.contains("open or design questions")
                && b.contains("ta dep add"),
            "filing discipline + dependencies: {b}"
        );
        assert!(
            b.contains("append progress to related tasks"),
            "cross-task notes: {b}"
        );
        assert!(b.contains("<field>=@-"), "stdin/file input: {b}");
        assert!(
            b.contains("unrelated to what you're starting"),
            "commit hygiene: {b}"
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
