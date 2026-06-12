//! e2e: `ta init`'s agent-integration block in CLAUDE.md / AGENTS.md.
//!
//! Drives the real binary to prove init writes a marker-delimited block, updates
//! existing files in place, creates AGENTS.md only when neither exists, stays
//! idempotent, and re-syncs to the current config.

mod common;
use common::*;

use std::fs;

/// With no agent file present, init creates AGENTS.md (the cross-tool default)
/// and not CLAUDE.md.
#[test]
fn init_creates_agents_md_when_none_exists() {
    let dir = fresh_dir("init-agents");
    init_repo(&dir);
    ta(&dir, &["init"]);

    let agents = fs::read_to_string(dir.join("AGENTS.md")).expect("AGENTS.md created");
    assert!(
        agents.contains("BEGIN TASKA INTEGRATION") && agents.contains("END TASKA INTEGRATION"),
        "marker-delimited block: {agents}"
    );
    assert!(
        agents.contains("ta prime"),
        "points at the full guide: {agents}"
    );
    assert!(
        !dir.join("CLAUDE.md").exists(),
        "does not also create CLAUDE.md"
    );
}

/// An existing file is updated IN PLACE (prior content preserved); a second run
/// is a byte-identical no-op, and AGENTS.md isn't created alongside it.
#[test]
fn init_updates_existing_file_in_place_and_is_idempotent() {
    let dir = fresh_dir("init-update");
    init_repo(&dir);
    fs::write(dir.join("CLAUDE.md"), "# Project\n\nHand-written notes.\n").unwrap();
    ta(&dir, &["init"]);

    let claude = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
    assert!(
        claude.starts_with("# Project"),
        "preserves prior content: {claude}"
    );
    assert!(
        claude.contains("BEGIN TASKA INTEGRATION"),
        "adds the block: {claude}"
    );
    assert!(
        !dir.join("AGENTS.md").exists(),
        "only the existing file is touched"
    );

    ta(&dir, &["init"]);
    let claude2 = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
    assert_eq!(
        claude2.matches("BEGIN TASKA INTEGRATION").count(),
        1,
        "no duplicate block: {claude2}"
    );
    assert_eq!(claude, claude2, "re-run is byte-identical");
}

/// The block is config-AGNOSTIC, so it does NOT change when the config changes -
/// renaming the status field leaves it byte-identical. (The dynamic, tailored
/// detail lives in `ta prime`, which the block points at.)
#[test]
fn init_block_is_config_agnostic() {
    let dir = fresh_dir("init-agnostic");
    init_repo(&dir);
    ta(&dir, &["init"]);
    let before = fs::read_to_string(dir.join("AGENTS.md")).unwrap();

    ta(&dir, &["config", "set", "workflow.status_field", "state"]);
    ta(&dir, &["init"]);
    let after = fs::read_to_string(dir.join("AGENTS.md")).unwrap();

    assert_eq!(
        before, after,
        "static block is unaffected by a config change"
    );
    assert!(
        before.contains("ta prime"),
        "points at the dynamic guide: {before}"
    );
    assert!(
        !before.contains("state="),
        "carries no status-field literal: {before}"
    );
}
