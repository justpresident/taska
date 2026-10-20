//! `columns` action: the store's column vocabulary, and the check every
//! column-name input goes through.
//!
//! A column name a command can't resolve is always a typo or a misconception,
//! never a request - an unknown `--sort` key used to fall back to `id` order and
//! an unknown `--columns` entry to render blank, both silently. So every surface
//! that takes a column name (`--columns`, `--sort`, filter criteria, `--if`
//! guards, `watch --columns`, the `[display]` keys `config set` writes) checks
//! it here and rejects an unknown one with a did-you-mean. Shell completion
//! offers the same vocabulary, so what completes is exactly what's accepted.

use std::collections::{BTreeSet, HashMap};

use crate::action::{materialize, Warning};
use crate::config::Config;
use crate::error::DynError;
use crate::model::{
    MutationEvent, OpType, TaskState, BLOCKED_BY_KEY, DEPS_KEY, ID_KEY, RESERVED_FIELD_KEYS,
    SUBTASKS_KEY, UNBLOCKS_KEY,
};
use crate::schema::{canonical_field_pairs, editable_field_names, nearest_known};
use crate::storage::EventStore;

/// Where a column name is used - it decides which names are valid there and
/// how an error names the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnUse {
    /// A `list` column (`--columns`). Relationship names render blank there -
    /// edges show under `deps` - so they're rejected.
    List,
    /// A `show` column: `show` also renders each relationship and inverse.
    Show,
    /// A sort key (`--sort`, `[display] sort`).
    Sort,
    /// A filter criterion field (`list`/`watch` criteria, `--if` guards):
    /// relationship types and inverses filter by target.
    Filter,
    /// A `[display] column_max_width` key.
    Width,
}

impl ColumnUse {
    const fn noun(self) -> &'static str {
        match self {
            Self::List | Self::Show | Self::Width => "column",
            Self::Sort => "sort column",
            Self::Filter => "filter field",
        }
    }

    const fn takes_relationships(self) -> bool {
        matches!(self, Self::Show | Self::Filter)
    }
}

/// The column names a store knows, in DISPLAY names.
#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    /// Every column: settable fields, `id`, `deps`, the enabled timestamps and
    /// the graph-computed columns.
    columns: BTreeSet<String>,
    /// Relationship type names and their inverses.
    relationships: BTreeSet<String>,
}

impl Vocabulary {
    /// The part of the vocabulary config alone determines: the built-in and
    /// computed columns, the status/type fields, `[display] columns`, and every
    /// declared `[task_types]` field. Enough to accept the common names without
    /// replaying the log.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let w = &config.workflow;
        let ts = &config.timestamps;
        let mut columns: BTreeSet<String> =
            [ID_KEY, DEPS_KEY, UNBLOCKS_KEY, BLOCKED_BY_KEY, SUBTASKS_KEY]
                .into_iter()
                .chain([
                    ts.create_time.as_str(),
                    ts.update_time.as_str(),
                    ts.close_time.as_str(),
                ])
                .chain([w.status_field.as_str(), w.type_field.as_str()])
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect();
        columns.extend(config.display.columns.iter().cloned());
        for def in config.task_types.types.values() {
            columns.extend(def.fields.keys().cloned());
        }
        let relationships = config
            .relationships
            .types
            .iter()
            .flat_map(|(name, def)| [name.clone(), def.inverse.clone()])
            .filter(|n| !n.is_empty())
            .collect();
        Self {
            columns,
            relationships,
        }
    }

    /// The full vocabulary over RAW (canonical) `state` and the retained `log`:
    /// [`Vocabulary::from_config`] plus every field a task uses and every field
    /// the retained history ever wrote - so a field just cleared from every task
    /// stays filterable (a `--if owner=` claim must keep working once nobody
    /// holds a claim). Only compaction forgets a field.
    #[must_use]
    pub fn build(
        config: &Config,
        state: &HashMap<String, TaskState>,
        log: &[MutationEvent],
    ) -> Self {
        let mut vocab = Self::from_config(config);
        let mut canonical = editable_field_names(state, config);
        for event in log {
            if matches!(
                event.op,
                OpType::Create | OpType::Update | OpType::Append | OpType::Add | OpType::Remove
            ) {
                canonical.extend(event.payload.keys().cloned());
            }
        }
        vocab.add_canonical(config, canonical);
        vocab
    }

    /// Add CANONICAL field names (as storage spells them), mapped to their
    /// display names - e.g. the fields a write is about to introduce, which a
    /// guard on that same write may name.
    pub fn add_canonical(&mut self, config: &Config, names: impl IntoIterator<Item = String>) {
        let pairs = canonical_field_pairs(&config.workflow);
        for name in names {
            if RESERVED_FIELD_KEYS.contains(&name.as_str()) {
                continue;
            }
            let display = pairs
                .iter()
                .find(|(_, canonical)| *canonical == name)
                .map_or_else(|| name.clone(), |(display, _)| (*display).clone());
            self.columns.insert(display);
        }
    }

    /// Whether every name is valid for `usage`.
    #[must_use]
    pub fn covers(&self, names: &[&str], usage: ColumnUse) -> bool {
        names.iter().all(|n| self.accepts(n, usage))
    }

    fn accepts(&self, name: &str, usage: ColumnUse) -> bool {
        self.columns.contains(name)
            || (usage.takes_relationships() && self.relationships.contains(name))
    }

    /// Reject the first name not valid for `usage`, with a did-you-mean.
    ///
    /// # Errors
    /// The unknown name, why it's rejected, and the nearest known name.
    pub fn check(&self, names: &[&str], usage: ColumnUse) -> Result<(), DynError> {
        names
            .iter()
            .find(|n| !self.accepts(n, usage))
            .map_or(Ok(()), |name| Err(self.unknown(name, usage).into()))
    }

    /// The error text for a name `usage` can't take - also the read-time warning
    /// text for an unknown CONFIGURED sort key.
    #[must_use]
    pub fn unknown(&self, name: &str, usage: ColumnUse) -> String {
        let noun = usage.noun();
        if self.relationships.contains(name) {
            return format!(
                "`{name}` is a relationship type, not a {noun} - relationships show under \
                 the `{DEPS_KEY}` column"
            );
        }
        let mut known = self.columns.clone();
        if usage.takes_relationships() {
            known.extend(self.relationships.iter().cloned());
        }
        // A close match is the actionable answer; with none, name them all, so
        // the message works in a store the user doesn't know by heart.
        let hint = nearest_known(name, &known).map_or_else(
            || {
                let all: Vec<String> = known.iter().map(|k| format!("`{k}`")).collect();
                format!(" (known: {})", all.join(", "))
            },
            |k| format!(" - did you mean `{k}`?"),
        );
        format!("unknown {noun} `{name}`: no task uses it and no schema declares it{hint}")
    }

    /// Every column (display names), for completion.
    #[must_use]
    pub const fn columns(&self) -> &BTreeSet<String> {
        &self.columns
    }

    /// Every filterable field: the columns plus relationship names, for completion.
    #[must_use]
    pub fn filter_fields(&self) -> BTreeSet<String> {
        self.columns.union(&self.relationships).cloned().collect()
    }
}

/// The store's vocabulary, replaying the log only when `names` aren't all
/// covered by the config alone (the common case costs nothing).
///
/// # Errors
/// Reading the store.
pub fn vocabulary_for(
    store: &impl EventStore,
    names: &[&str],
    usage: ColumnUse,
) -> Result<Vocabulary, DynError> {
    let config = store.config();
    let quick = Vocabulary::from_config(config);
    if quick.covers(names, usage) {
        return Ok(quick);
    }
    full_vocabulary(store)
}

/// The store's full vocabulary (always replays the log).
///
/// # Errors
/// Reading the store.
pub fn full_vocabulary(store: &impl EventStore) -> Result<Vocabulary, DynError> {
    let config = store.config();
    let baseline = store.load_baseline()?;
    let log = store.load_mutations()?;
    let state = materialize(config, &baseline, &log);
    Ok(Vocabulary::build(config, &state, &log))
}

/// Check `names` against the store's vocabulary for `usage`.
///
/// # Errors
/// An unknown name (with a did-you-mean), or reading the store.
pub fn check_columns(
    store: &impl EventStore,
    names: &[&str],
    usage: ColumnUse,
) -> Result<(), DynError> {
    if names.is_empty() {
        return Ok(());
    }
    vocabulary_for(store, names, usage)?.check(names, usage)
}

/// A sort spec ([`crate::model::parse_sort`]) and where it came from - which
/// decides what an unknown key costs (see [`check_sort`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortSpec<'a> {
    /// Asked for on this call (`--sort`): an unknown key is an error.
    Requested(&'a str),
    /// The configured `[display] sort`: an unknown key only warns.
    Configured(&'a str),
}

impl<'a> SortSpec<'a> {
    /// The spec text.
    #[must_use]
    pub const fn spec(self) -> &'a str {
        match self {
            Self::Requested(s) | Self::Configured(s) => s,
        }
    }
}

/// Check sort keys against `vocab`, by where the spec came from.
///
/// An unknown key in a [`SortSpec::Requested`] sort is an error; in the
/// [`SortSpec::Configured`] one it's a [`Warning`] instead, so a config typo
/// can't lock out the reads that would expose it (ordering just skips the key).
///
/// # Errors
/// An unknown requested sort key.
pub fn check_sort(
    vocab: &Vocabulary,
    sorts: &[&str],
    spec: SortSpec,
) -> Result<Vec<Warning>, DynError> {
    if let SortSpec::Requested(_) = spec {
        vocab.check(sorts, ColumnUse::Sort)?;
        return Ok(Vec::new());
    }
    Ok(sorts
        .iter()
        .filter(|c| !vocab.accepts(c, ColumnUse::Sort))
        .map(|c| Warning::ConfigSort(vocab.unknown(c, ColumnUse::Sort)))
        .collect())
}
