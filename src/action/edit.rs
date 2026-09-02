//! The **edit form**: the document a frontend shows for one task, and the write
//! a saved document means.
//!
//! `ta edit` is the CLI's `$EDITOR` round-trip, but nothing here knows that: a
//! frontend gets [`EditForm::template`], presents it however it likes (a temp
//! file, a TUI panel, a web form), and hands the saved fields back to
//! [`EditForm::preview`], which resolves them into a write payload and validates
//! it against the pre-edit snapshot - the same diagnostics the real write would
//! produce, *before* the store lock is taken. [`EditForm::apply`] then appends
//! through `write::create`/`write::update`.
//!
//! **This module decides what a save MEANS, never what a value means.** A form
//! is a partial view of a task, so exactly two things can be read off it: a
//! field whose value the user changed is a write, and a *stored* field whose
//! line the user deleted is an unset. Everything else - that `""` is the unset
//! value, that a declared default fills an absent field, that a required field
//! must be present, that re-asserting a value is a no-op - is the repository's
//! law, applied downstream by `schema` and `action::write`. Values pass through
//! this module untouched so those rules stay in one place.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value};

use crate::action::{materialize, write};
use crate::config::{Config, UntypedTasks};
use crate::error::DynError;
use crate::model::{MutationEvent, TaskState, STATUS_KEY, TASK_TYPE_KEY};
use crate::schema::{
    build_field_events, canonicalize_fields, coerce_event_fields, editable_field_names,
    introduced_field_names, known_field_names, rename_to_display, schema_default_stamps,
    vet_events, FieldOps,
};
use crate::storage::EventStore;

/// Which write a saved form resolves to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    /// The task exists; the save updates it.
    Update,
    /// The task doesn't exist; the save creates it.
    Create,
}

/// What a saved document means.
pub enum Preview {
    /// The document carries no fields at all - saved empty, or nothing but
    /// comments. The documented discard, never a mass-unset: a form the user
    /// emptied is an abort, not "delete every field".
    Empty,
    /// The CANONICAL payload the save resolves to (empty = nothing changed),
    /// plus the brand-new field names it introduces (not used by any task).
    Ready {
        set: Map<String, Value>,
        new_fields: Vec<String>,
    },
}

/// One task's editable form, opened against a fixed pre-edit snapshot.
///
/// Keys are the configured DISPLAY names throughout - what the user sees is what
/// they type back; the canonical rename happens in [`preview`](Self::preview),
/// on the way to the write.
pub struct EditForm {
    /// The document to show: every field the user may set, with stored values,
    /// applicable defaults, and `""` for the rest.
    pub template: Map<String, Value>,
    /// What the task actually stores today (empty when creating) - the baseline
    /// a save is diffed against.
    stored: Map<String, Value>,
    /// State as of when the form opened; the preview validates against it.
    snapshot: HashMap<String, TaskState>,
    mode: EditMode,
    id: String,
}

impl EditForm {
    /// Open the form for `id`. An existing task updates; an absent one creates
    /// only with `create_missing`, which conversely rejects an id already taken
    /// (before the user is shown an editor they'd only lose).
    pub fn open(store: &impl EventStore, id: &str, create_missing: bool) -> Result<Self, DynError> {
        // RAW snapshot (canonical keys, no injected timestamps/computed columns),
        // so the stored baseline is exact; the editable vocabulary is added below.
        let config = store.config();
        let snapshot = materialize(config, &store.load_baseline()?, &store.load_mutations()?);
        let (stored_fields, mode) = match (snapshot.get(id), create_missing) {
            (Some(_), true) => return Err(format!("task `{id}` already exists").into()),
            (Some(task), false) => (task.custom_fields.clone(), EditMode::Update),
            (None, true) => (Map::new(), EditMode::Create),
            (None, false) => return Err(format!("no task `{id}`").into()),
        };

        let mut stored = stored_fields.clone();
        rename_to_display(&mut stored, &config.workflow);
        Ok(Self {
            template: build_template(&stored_fields, &snapshot, config, mode),
            stored,
            snapshot,
            mode,
            id: id.to_string(),
        })
    }

    /// Whether this form creates or updates.
    pub const fn mode(&self) -> EditMode {
        self.mode
    }

    /// Resolve a saved document into its canonical write payload and validate it,
    /// so a frontend can surface real diagnostics (syntax is its own concern;
    /// canonical-name conflicts and schema violations are ours) before writing.
    pub fn preview(
        &self,
        edited: &Map<String, Value>,
        config: &Config,
    ) -> Result<Preview, DynError> {
        if edited.is_empty() {
            return Ok(Preview::Empty);
        }
        let mut set = self.payload(edited);
        canonicalize_fields(&mut set, &config.workflow)?;
        if set.is_empty() {
            return Ok(Preview::Ready {
                set,
                new_fields: Vec::new(),
            });
        }

        let events = self.draft_events(&set, config)?;
        vet_events(&events, &self.snapshot, config)?;

        let known = known_field_names(&self.snapshot, config);
        let mut new_fields = introduced_field_names(&events, &known);
        // Match `create`'s typo-guard grace: the first task defines the initial
        // field vocabulary without requiring an interactive confirmation.
        if self.mode == EditMode::Create && self.snapshot.is_empty() {
            new_fields.clear();
        }
        Ok(Preview::Ready { set, new_fields })
    }

    /// Append a previewed payload through the shared write path, which re-runs
    /// every check under the store lock (the editor ran outside it).
    pub fn apply(
        &self,
        store: &impl EventStore,
        payload: Map<String, Value>,
        allow_new_fields: bool,
    ) -> Result<write::WriteOutcome, DynError> {
        match self.mode {
            EditMode::Update => {
                write::update(store, &self.id, &set_only(payload), allow_new_fields, &[])
            }
            EditMode::Create => {
                write::create(store, &self.id, payload, &Map::new(), allow_new_fields)
            }
        }
    }

    /// What a saved document writes, in DISPLAY names.
    ///
    /// A form is a partial view, so only two things are readable from it: a value
    /// the user changed, and a *stored* field whose line they deleted (the unset).
    /// A template-only line - a suggested default or a `""` placeholder - has no
    /// stored value behind it, so leaving it alone writes nothing and deleting it
    /// unsets nothing; to suppress a default the user blanks it (`field = ""`)
    /// like anywhere else in the repository, and `schema` takes it from there.
    fn payload(&self, edited: &Map<String, Value>) -> Map<String, Value> {
        let mut set = Map::new();
        for (key, value) in edited {
            let unchanged_stored = self.stored.get(key) == Some(value);
            let untouched_template =
                !self.stored.contains_key(key) && self.template.get(key) == Some(value);
            if !unchanged_stored && !untouched_template {
                set.insert(key.clone(), value.clone());
            }
        }
        for key in self.stored.keys() {
            if !edited.contains_key(key) {
                set.insert(key.clone(), Value::Null);
            }
        }
        set
    }

    /// The events this save would append, drafted exactly as the write path
    /// drafts them (`write::create_draft` for a create, `build_field_events` for
    /// an update) so the preview can't diverge from what follows it.
    fn draft_events(
        &self,
        set: &Map<String, Value>,
        config: &Config,
    ) -> Result<Vec<MutationEvent>, DynError> {
        match self.mode {
            EditMode::Update => {
                build_field_events(&self.id, &set_only(set.clone()), &self.snapshot, config)
            }
            EditMode::Create => {
                let mut events = vec![write::create_draft(config, &self.id, set.clone())];
                coerce_event_fields(&mut events, &Map::new(), &self.snapshot, config);
                Ok(events)
            }
        }
    }
}

/// Build the form's document: every field the user may set, in DISPLAY names.
///
/// The task's real content first - the defaults that apply to it, overlaid by its
/// own stored values - renamed to display names, and only THEN filled out with
/// `""` placeholders for the rest of the store's editable vocabulary. That order
/// matters: a store whose display name for one canonical key collides with a
/// field some task already stores under that name (grandfathered data, or a
/// config rename after the fact) would otherwise have its real value overwritten
/// by a blank.
fn build_template(
    stored_fields: &Map<String, Value>,
    snapshot: &HashMap<String, TaskState>,
    config: &Config,
    mode: EditMode,
) -> Map<String, Value> {
    let touched = BTreeSet::new();
    let mut content = Map::new();
    match mode {
        EditMode::Update => {
            for (key, value) in
                schema_default_stamps(Some(stored_fields), &Map::new(), &touched, config)
            {
                content.insert(key, value);
            }
        }
        EditMode::Create => {
            if !config.workflow.default_status.is_empty() {
                content.insert(
                    STATUS_KEY.to_string(),
                    Value::String(config.workflow.default_status.clone()),
                );
            }
            // A store that requires a type and declares exactly one leaves no
            // choice to make, so suggesting it is unambiguous.
            if matches!(config.workflow.untyped_tasks, UntypedTasks::Deny) {
                let mut types = config.task_types.types.keys();
                if let (Some(type_name), None) = (types.next(), types.next()) {
                    content.insert(TASK_TYPE_KEY.to_string(), Value::String(type_name.clone()));
                }
            }
            for (key, value) in schema_default_stamps(None, &content, &touched, config) {
                content.insert(key, value);
            }
        }
    }
    content.extend(stored_fields.clone());
    rename_to_display(&mut content, &config.workflow);

    let mut blanks: Map<String, Value> = editable_field_names(snapshot, config)
        .into_iter()
        .map(|name| (name, Value::String(String::new())))
        .collect();
    rename_to_display(&mut blanks, &config.workflow);
    for (name, blank) in blanks {
        content.entry(name).or_insert(blank);
    }
    content
}

/// A [`FieldOps`] carrying only `set` fields - a form never appends or subtracts.
fn set_only(set: Map<String, Value>) -> FieldOps {
    FieldOps {
        set,
        append: Vec::new(),
        subtract: Vec::new(),
        raw: Map::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::task;
    use serde_json::json;

    fn map(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// A form with an explicit stored/template pair, bypassing a real store.
    fn form(mode: EditMode, stored: &[(&str, Value)], template: &[(&str, Value)]) -> EditForm {
        EditForm {
            template: map(template),
            stored: map(stored),
            snapshot: HashMap::new(),
            mode,
            id: "t".to_string(),
        }
    }

    #[test]
    fn payload_detects_change_add_and_remove() {
        let stored = &[
            ("title", json!("old")),
            ("priority", json!(1)),
            ("drop", json!("x")),
        ];
        let form = form(EditMode::Update, stored, stored);
        let set = form.payload(&map(&[
            ("title", json!("new")), // changed
            ("priority", json!(1)),  // unchanged -> skipped
            ("added", json!(9)),     // new
                                     // "drop" removed
        ]));

        assert_eq!(set.get("title"), Some(&json!("new")));
        assert_eq!(set.get("added"), Some(&json!(9)));
        assert_eq!(set.get("drop"), Some(&Value::Null), "removed field -> null");
        assert!(
            !set.contains_key("priority"),
            "unchanged field is not written"
        );
    }

    #[test]
    fn deleting_a_template_only_line_writes_nothing() {
        // Only a STORED field can be unset; a suggested default and a blank
        // placeholder have no value behind them, so their absence means nothing.
        let form = form(
            EditMode::Update,
            &[("title", json!("old"))],
            &[
                ("status", json!("todo")),
                ("points", json!(3)),
                ("title", json!("old")),
                ("owner", json!("")),
            ],
        );

        let set = form.payload(&map(&[("title", json!("old"))]));
        assert!(
            set.is_empty(),
            "deleting suggested defaults and placeholders is not a write: {set:?}"
        );

        let set = form.payload(&map(&[("status", json!("todo"))]));
        assert_eq!(
            set.get("title"),
            Some(&Value::Null),
            "deleting a stored field unsets it"
        );
    }

    #[test]
    fn blanking_a_value_passes_the_empty_string_through() {
        // `""` is the repository's unset value (`schema` turns it into an unset
        // for an optional field, keeps it for a required one). The form must not
        // second-guess that - it just carries what the user typed.
        let form = form(
            EditMode::Update,
            &[("title", json!("old"))],
            &[("title", json!("old")), ("owner", json!(""))],
        );
        let set = form.payload(&map(&[("title", json!("")), ("owner", json!(""))]));

        assert_eq!(
            set.get("title"),
            Some(&json!("")),
            "blanked value is a write"
        );
        assert!(
            !set.contains_key("owner"),
            "an untouched placeholder is still not a write"
        );
    }

    #[test]
    fn create_keeps_defaults_out_of_the_payload_until_touched() {
        let template = &[
            ("status", json!("todo")),
            ("title", json!("")),
            ("type", json!("story")),
            ("rank", json!("")),
        ];
        let form = form(EditMode::Create, &[], template);

        assert!(
            form.payload(&map(template)).is_empty(),
            "an untouched form creates nothing"
        );

        let mut edited = map(template);
        edited.insert("title".to_string(), json!("New task"));
        let payload = form.payload(&edited);
        assert_eq!(payload.get("title"), Some(&json!("New task")));
        assert!(
            !payload.contains_key("status") && !payload.contains_key("type"),
            "untouched suggestions stay out, so `write::create` stamps them: {payload:?}"
        );
        assert!(!payload.contains_key("rank"));
    }

    #[test]
    fn create_never_suppresses_a_default_by_omission() {
        // Deleting the status line (or retyping the whole form without it) must
        // NOT reach `write::create` as an explicit null, which would suppress the
        // workflow default and leave a task no status filter can ever match.
        let form = form(
            EditMode::Create,
            &[],
            &[("status", json!("todo")), ("title", json!(""))],
        );
        let payload = form.payload(&map(&[("title", json!("Created in editor"))]));

        assert_eq!(payload.get("title"), Some(&json!("Created in editor")));
        assert!(
            !payload.contains_key("status"),
            "an absent line must not become a null: {payload:?}"
        );
    }

    #[test]
    fn template_blanks_never_overwrite_a_stored_field() {
        // `status_field = "priority"` renames the canonical `status` onto a name
        // this task already stores an ordinary value under (grandfathered before
        // the rename). The real value must survive; the blank must not land.
        let config: Config = toml::from_str(
            r#"
[workflow]
status_field = "priority"
"#,
        )
        .unwrap();
        let stored = map(&[("priority", json!(2))]);
        let mut snapshot = HashMap::new();
        snapshot.insert("t".to_string(), task("t", &[], &[("priority", json!(2))]));

        let template = build_template(&stored, &snapshot, &config, EditMode::Update);
        assert_eq!(
            template.get("priority"),
            Some(&json!(2)),
            "a stored value must not be clobbered by a placeholder: {template:?}"
        );
    }

    #[test]
    fn create_template_prefills_editable_fields_and_known_defaults() {
        let config: Config = toml::from_str(
            r#"
[workflow]
untyped_tasks = "deny"

[task_types.story.fields]
title = { type = "string", required = true }
points = { type = "uint", default = 3 }
"#,
        )
        .unwrap();
        let snapshot: HashMap<String, TaskState> = HashMap::new();
        let template = build_template(&Map::new(), &snapshot, &config, EditMode::Create);

        assert_eq!(template.get("status"), Some(&json!("todo")));
        assert_eq!(template.get("type"), Some(&json!("story")));
        assert_eq!(template.get("title"), Some(&json!("")));
        assert_eq!(template.get("points"), Some(&json!(3)));
        for reserved in ["id", "deps", "create_time", "update_time", "close_time"] {
            assert!(
                !template.contains_key(reserved),
                "{reserved} is not editable"
            );
        }
    }
}
