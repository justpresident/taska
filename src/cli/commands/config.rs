//! `ta config get|set|list` - view or change `.taska/config.toml` by dotted key.
//!
//! The data work (resolving keys, flattening, validating, the comment-preserving
//! file edit) lives in [`crate::action::config`]; this file is the clap surface
//! plus the git-config-style rendering.

use clap::Subcommand;

use crate::config::Config;
use crate::error::DynError;
use crate::storage::{EventStore, FileStore};

/// `ta config` subcommands: git-config-style get/set/list over dotted keys.
#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print one effective value: `ta config get compaction.keep_events`
    Get { key: String },
    /// Set a value, validating the result: `ta config set merge.on_conflict ours`
    Set { key: String, value: String },
    /// Print every effective config value as `dotted.key = value`
    List,
    /// Check the config against the task graph: `ta config validate`
    Validate,
}

/// Dispatch `ta config get|set|list|validate`.
pub fn cmd_config(store: &FileStore, action: ConfigAction) -> Result<(), DynError> {
    match action {
        ConfigAction::Get { key } => cmd_config_get(store.config(), &key),
        ConfigAction::List => cmd_config_list(store.config()),
        ConfigAction::Set { key, value } => cmd_config_set(store, &key, &value),
        ConfigAction::Validate => cmd_config_validate(store),
    }
}

/// Validate the effective config against the materialized task graph, reporting
/// every problem found (or confirming it's clean). Schema NON-conformance of
/// existing tasks is listed as warnings, not errors - grandfathered data is
/// read-tolerated by design.
fn cmd_config_validate(store: &FileStore) -> Result<(), DynError> {
    let report = crate::action::config::validate(store)?;
    crate::cli::print_warnings(&report.read_warnings);
    for line in &report.nonconformance {
        eprintln!("warning: {line}");
    }
    if report.nonconformance.is_empty() {
        println!("Config OK ({} task(s) checked).", report.task_count);
    } else {
        println!(
            "Config OK ({} task(s) checked; {} not conforming to their task-type schema - \
             writes to those tasks must bring them into conformance).",
            report.task_count,
            report.nonconformance.len()
        );
    }
    Ok(())
}

/// Print one effective config value addressed by a dotted key.
fn cmd_config_get(cfg: &Config, key: &str) -> Result<(), DynError> {
    println!(
        "{}",
        show_config_value(&crate::action::config::get(cfg, key)?)
    );
    Ok(())
}

/// Print every effective config value as sorted `dotted.key = value` lines.
fn cmd_config_list(cfg: &Config) -> Result<(), DynError> {
    for (k, v) in crate::action::config::list(cfg)? {
        println!("{k} = {}", show_config_value(&v));
    }
    Ok(())
}

/// Set one config value (validated, comment-preserving) and confirm it.
fn cmd_config_set(store: &FileStore, key: &str, raw: &str) -> Result<(), DynError> {
    let shown = crate::action::config::set(store, key, raw)?;
    println!("Set {key} = {shown}");
    Ok(())
}

/// Render a leaf config value git-config-style: bare for strings, TOML form
/// (numbers, bools, arrays) otherwise.
fn show_config_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;

    #[test]
    fn show_renders_strings_bare_and_others_as_toml() {
        // git-config style: a string renders bare, everything else its TOML form.
        assert_eq!(show_config_value(&toml::Value::from("status")), "status");
        assert_eq!(show_config_value(&toml::Value::from(5000)), "5000");
        assert_eq!(show_config_value(&toml::Value::from(true)), "true");
    }
}
