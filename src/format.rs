//! Presentation: how tasks are turned into text.
//!
//! The selected columns (`--columns`/`--full`/config) decide *which* fields
//! appear; `--format` decides only *how* they print, and every format shares the
//! same column order. This module owns the display flags, column resolution,
//! sorting, and the human/json/jsonl renderers; the command handlers feed it a
//! task slice and print the result.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::io::IsTerminal;

use clap::{Args, ValueEnum};
use serde_json::Value;

use crate::config::{DisplayConfig, Layout};
use crate::model::TaskState;

/// Wrap `text` in an ANSI SGR sequence when `on`, else return it unchanged. Uses
/// the terminal's NAMED 16-color palette (which the user's theme remaps for
/// light/dark), never hardcoded 24-bit RGB. Shared with `dep tree` coloring.
pub(crate) fn sgr(text: &str, code: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The SGR code a value column is painted with in human output: the built-in
/// `id` (cyan) plus a `status` field (green). Everything else is left plain
/// (headers/labels are bolded separately; `deps` styles itself per type group,
/// see [`group_sgr`]).
fn column_sgr(column: &str) -> Option<&'static str> {
    match column {
        "id" => Some("36"),     // cyan
        "status" => Some("32"), // green
        _ => None,
    }
}

/// The SGR code for one deps-cell type group: a relationship type that gates
/// readiness (the blocker/hierarchy kinds) renders bold, an informational one
/// dim — so what blocks vs what's just related is visible at a glance.
const fn group_sgr(gates: bool) -> &'static str {
    if gates {
        "1" // bold
    } else {
        "2" // dim
    }
}

/// Whether human output should be colored: not `--no-color`, `NO_COLOR` unset,
/// and stdout is a TTY (so pipes, redirects, and `--format json`/`jsonl` stay
/// clean). The json/jsonl renderers never color regardless.
pub(crate) fn want_color(no_color: bool) -> bool {
    !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Output format for the listing commands. `--format` changes only *how* tasks
/// are rendered, never *which* fields show — that is `--columns`/`--full`/config.
#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    /// Aligned human table.
    Human,
    /// Pretty JSON array.
    Json,
    /// Newline-delimited JSON (one object per line).
    Jsonl,
}

/// The machine-format + color flags EVERY output command shares. Tabular
/// commands flatten this into [`DisplayArgs`]; structured commands (`status`,
/// `dep *`) take it directly and route through [`emit`], so `--format`/`--no-color`
/// behave identically everywhere.
#[derive(Args, Clone)]
pub(crate) struct OutputArgs {
    /// Output format: human, json (pretty array/object), or jsonl (NDJSON)
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
    /// Disable ANSI color (also auto-disabled when stdout is not a TTY)
    #[arg(long)]
    pub(crate) no_color: bool,
}

/// Display flags shared by `list` and `show`: the common [`OutputArgs`] plus the
/// tabular extras (columns/sort/layout/full).
#[derive(Args, Clone)]
pub(crate) struct DisplayArgs {
    #[command(flatten)]
    pub(crate) output: OutputArgs,
    /// Show every field, not just the configured columns
    #[arg(long)]
    pub(crate) full: bool,
    /// Columns to show, overriding config. Built-ins `id`, `deps`; computed
    /// `create_time`/`update_time`/`close_time`, `unblocks`, `blocked_by`,
    /// `subtasks`; plus any task field. E.g. --columns id,status,unblocks
    #[arg(long, value_delimiter = ',')]
    pub(crate) columns: Option<Vec<String>>,
    /// Sort rows by this column (id, deps, or any field), overriding config
    #[arg(long)]
    pub(crate) sort: Option<String>,
    /// Reverse the sort order (descending)
    #[arg(long)]
    pub(crate) reverse: bool,
    /// Human layout: `table` (aligned columns) or `list` (vertical record); the
    /// per-command default lives in `[display]` (`list_layout`/`show_layout`)
    #[arg(long, value_enum)]
    pub(crate) layout: Option<Layout>,
}

/// Emit `value` per the chosen format: the prebuilt `human` string, pretty JSON,
/// or NDJSON (one line per top-level array element, else one compact line). Color
/// is the caller's concern (human output only) — json/jsonl are never colored.
/// The single output dispatch for the structured commands (`status`, `dep *`).
pub(crate) fn emit(out: &OutputArgs, human: &str, value: &Value) {
    match out.format {
        OutputFormat::Human => println!("{human}"),
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
        }
        OutputFormat::Jsonl => match value {
            Value::Array(items) => {
                for item in items {
                    println!("{}", serde_json::to_string(item).unwrap_or_default());
                }
            }
            other => println!("{}", serde_json::to_string(other).unwrap_or_default()),
        },
    }
}

/// Sort a collected task set by the display args and print it, with `empty` as
/// the human placeholder for no rows. `blockers` is the readiness-gating
/// relationship-type set (`RelationshipConfig::blocker_types`), used only to
/// style the deps cell. The shared print tail of `list` (plain, `--open`, or
/// `--ready`), which differs only in how it gathers the tasks.
pub(crate) fn print_tasks(
    mut tasks: Vec<&TaskState>,
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    blockers: &BTreeSet<String>,
    empty: &str,
) {
    sort_tasks(&mut tasks, display, cfg);
    println!("{}", render(&tasks, display, cfg, blockers, empty));
}

/// Render tasks per the display args. The selected columns decide *which* fields
/// appear; `--format` decides only how they print, and both formats share the
/// same field order.
fn render(
    tasks: &[&TaskState],
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    blockers: &BTreeSet<String>,
    empty: &str,
) -> String {
    // Only the human table needs an explicit empty placeholder; json/jsonl render
    // their own empty forms (`[]` / no lines).
    if display.output.format == OutputFormat::Human && tasks.is_empty() {
        return empty.to_string();
    }
    let columns = resolve_columns(display, cfg, tasks);
    render_rows(tasks, &columns, display, cfg, blockers)
}

/// Dispatch the chosen `--format` over an already-resolved column set. Shared by
/// the multi-row `render` path and single-task `show`, so a new output format is
/// wired in exactly one place. `blockers` only styles the human deps cell —
/// json/jsonl carry the typed map itself, so kinds stay out of machine output.
pub(crate) fn render_rows(
    tasks: &[&TaskState],
    columns: &[String],
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    blockers: &BTreeSet<String>,
) -> String {
    match display.output.format {
        OutputFormat::Json => render_json(tasks, columns),
        OutputFormat::Jsonl => render_jsonl(tasks, columns),
        OutputFormat::Human => {
            let color = want_color(display.output.no_color);
            match display.layout.unwrap_or(Layout::Table) {
                Layout::Table => render_human(
                    tasks,
                    columns,
                    &truncation_caps(columns, display, cfg),
                    color,
                    blockers,
                ),
                Layout::List => render_records(tasks, columns, color, blockers),
            }
        }
    }
}

/// A vertical record per task (the `list` layout), records separated by a blank
/// line. Each record is the same view `show` produces, so the two share a format.
fn render_records(
    tasks: &[&TaskState],
    columns: &[String],
    color: bool,
    blockers: &BTreeSet<String>,
) -> String {
    tasks
        .iter()
        .map(|t| render_record(t, columns, color, blockers))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Sort `tasks` in place by the effective sort column (`--sort`, else the
/// configured default), ascending, with `id` as a stable tiebreaker; `--reverse`
/// flips the result. The column may be `id`, `deps`, or any field (including the
/// injected computed timestamps). Rows lacking the column sort last (ascending);
/// an empty or unknown column leaves only the `id` tiebreak, i.e. orders by id.
fn sort_tasks(tasks: &mut [&TaskState], display: &DisplayArgs, cfg: &DisplayConfig) {
    let column = display.sort.as_deref().unwrap_or(cfg.sort.as_str());
    tasks.sort_by(|a, b| task_cmp(a, b, column));
    if display.reverse {
        tasks.reverse();
    }
}

/// Compare two tasks by one column, ascending, with `id` as the stable
/// tiebreaker — a present value sorts before a missing one. The shared ordering
/// behind `list`'s `--sort` and `dep tree`'s sibling sort. `--reverse` flips the
/// result at the call site.
pub(crate) fn task_cmp(a: &TaskState, b: &TaskState, column: &str) -> Ordering {
    let ord = match (cell_value(a, column), cell_value(b, column)) {
        (Some(x), Some(y)) => cmp_json(&x, &y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    ord.then_with(|| a.id.cmp(&b.id))
}

/// The value of `column` for a task as a JSON `Value` — the single source of
/// truth shared by JSON output, human rendering, and sorting. `id` is the id
/// string, `deps` the task's typed relationships map (`{type: [targets…]}` —
/// every edge, keyed by relationship type, `{}` when none), and anything else a
/// custom or computed field. `None` only for a missing custom field (the
/// built-ins always resolve), which is how JSON omits absent fields and sorting
/// orders them last.
fn cell_value(task: &TaskState, column: &str) -> Option<Value> {
    match column {
        "id" => Some(Value::String(task.id.clone())),
        "deps" => Some(Value::Object(
            task.relationships
                .iter()
                .map(|(rel, targets)| {
                    let arr = targets.iter().cloned().map(Value::String).collect();
                    (rel.clone(), Value::Array(arr))
                })
                .collect(),
        )),
        _ => task.custom_fields.get(column).cloned(),
    }
}

/// The deps cell's type groups, in map (= JSON key) order: one
/// `(type, "type: target, target")` pair per relationship type. The human
/// renderers style each group by its type's kind ([`group_sgr`]); the plain
/// texts joined by `"; "` are [`human_cell`]'s form of the column.
fn deps_groups(task: &TaskState) -> Vec<(String, String)> {
    task.relationships
        .iter()
        .map(|(rel, targets)| (rel.clone(), format!("{rel}: {}", targets.join(", "))))
        .collect()
}

/// Build the deps table cell: the type groups joined by `"; "`, truncated on the
/// PLAIN text to `cap` (0 = no limit, ellipsis when cut — mirroring [`truncate`],
/// which can't be reused because cutting styled text would slice escape
/// sequences), each surviving group wrapped per its kind when `color`. Returns
/// the possibly SGR-laden cell together with its plain display width, so the
/// table pads on visible characters.
fn deps_cell(
    task: &TaskState,
    blockers: &BTreeSet<String>,
    cap: usize,
    color: bool,
) -> (String, usize) {
    let groups = deps_groups(task);
    let style = |rel: &str, text: &str| sgr(text, group_sgr(blockers.contains(rel)), color);
    let total = groups.iter().map(|(_, t)| t.chars().count()).sum::<usize>()
        + groups.len().saturating_sub(1) * 2;
    if cap == 0 || total <= cap {
        let cell = groups
            .iter()
            .map(|(rel, text)| style(rel, text))
            .collect::<Vec<_>>()
            .join("; ");
        return (cell, total);
    }
    // Spend the cap-1 budget group by group (the last char is the `…`), cutting
    // the group that crosses the boundary and dropping the rest.
    let budget = cap - 1;
    let mut used = 0;
    let mut cell = String::new();
    for (i, (rel, text)) in groups.iter().enumerate() {
        let sep = if i == 0 { 0 } else { 2 };
        if used + sep >= budget {
            break;
        }
        if i > 0 {
            cell.push_str("; ");
            used += sep;
        }
        let piece: String = text.chars().take(budget - used).collect();
        used += piece.chars().count();
        cell.push_str(&style(rel, &piece));
        if piece.chars().count() < text.chars().count() {
            break;
        }
    }
    cell.push('…');
    (cell, used + 1)
}

/// A total order over heterogeneous JSON scalars: numbers compare numerically,
/// strings/bools by their natural order, and any mismatch falls back to a stable
/// per-type rank then the value's string form — so a column holding mixed types
/// still sorts deterministically.
fn cmp_json(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => value_rank(a)
            .cmp(&value_rank(b))
            .then_with(|| a.to_string().cmp(&b.to_string())),
    }
}

/// Stable per-type ordinal so values of different JSON types compare consistently.
const fn value_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// The per-column truncation cap, one entry per column (0 = no limit, which
/// `truncate` already honors). `--full` prints everything untruncated, so every
/// column gets cap 0. Otherwise a column listed in `[display.column_max_width]`
/// uses its own width and the rest fall back to the global `max_width`.
fn truncation_caps(columns: &[String], display: &DisplayArgs, cfg: &DisplayConfig) -> Vec<usize> {
    columns
        .iter()
        .map(|c| {
            if display.full {
                0
            } else {
                cfg.column_max_width
                    .get(c)
                    .copied()
                    .unwrap_or(cfg.max_width)
            }
        })
        .collect()
}

/// The column names this display will *reference* — the sort key plus the
/// columns it will show (explicit `--columns`, else the configured default).
/// `--full` is excluded because it shows only fields already on the task; a
/// caller uses this to inject a computed column (e.g. `unblocks`/`blocked_by`) only
/// when it's actually needed, leaving default/`--full`/json output untouched.
pub(crate) fn referenced_columns(display: &DisplayArgs, cfg: &DisplayConfig) -> Vec<String> {
    let mut refs = vec![display.sort.clone().unwrap_or_else(|| cfg.sort.clone())];
    if let Some(cols) = &display.columns {
        refs.extend(cols.iter().cloned());
    } else if !display.full {
        refs.extend(cfg.columns.iter().cloned());
    }
    refs
}

/// Decide the columns: `--full` (the canonical full order), else an explicit
/// `--columns`, else the configured default.
fn resolve_columns(
    display: &DisplayArgs,
    cfg: &DisplayConfig,
    tasks: &[&TaskState],
) -> Vec<String> {
    if display.full {
        full_columns(tasks, cfg)
    } else if let Some(cols) = &display.columns {
        cols.clone()
    } else {
        cfg.columns.clone()
    }
}

/// The canonical column order for an all-fields view (`--full` and `show`'s
/// default): the configured `columns` that are actually present, in their exact
/// configured order — so `deps` keeps its slot — then every other present field
/// sorted alphabetically. The built-ins `id`/`deps` are always covered. A
/// configured column that no task in the view has is dropped, so a single-task
/// `show` and `--full` never pad with empty columns. Both human and JSON
/// rendering consume this same order, so their columns match.
pub(crate) fn full_columns(tasks: &[&TaskState], cfg: &DisplayConfig) -> Vec<String> {
    // Every field present across the view, plus the always-shown built-ins.
    let mut present: BTreeSet<&str> = BTreeSet::from(["id", "deps"]);
    for t in tasks {
        present.extend(t.custom_fields.keys().map(String::as_str));
    }
    // Configured columns that are present, in configured order...
    let mut cols: Vec<String> = cfg
        .columns
        .iter()
        .filter(|c| present.contains(c.as_str()))
        .cloned()
        .collect();
    // ...then the remaining present fields (incl. id/deps if unconfigured),
    // alphabetically (BTreeSet) for a deterministic tail.
    let listed: HashSet<&str> = cols.iter().map(String::as_str).collect();
    let tail: Vec<String> = present
        .into_iter()
        .filter(|f| !listed.contains(f))
        .map(String::from)
        .collect();
    drop(listed);
    cols.extend(tail);
    cols
}

/// Render the aligned human table. `caps[i]` is the truncation width for column
/// `i` (0 = no limit); the caller derives it from config/`--full` per column.
/// When `color`, headers are bolded, id/status cells get their palette colors,
/// and the deps cell carries per-type-group styling. Each cell travels with its
/// plain display width, so alignment is exact even around escape sequences.
fn render_human(
    tasks: &[&TaskState],
    columns: &[String],
    caps: &[usize],
    color: bool,
    blockers: &BTreeSet<String>,
) -> String {
    let headers: Vec<(String, usize)> = columns
        .iter()
        .map(|c| {
            let h = c.to_uppercase();
            let w = h.chars().count();
            (h, w)
        })
        .collect();
    let rows: Vec<Vec<(String, usize)>> = tasks
        .iter()
        .map(|t| {
            columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if c == "deps" {
                        deps_cell(t, blockers, caps[i], color)
                    } else {
                        let cell = truncate(&human_cell(t, c), caps[i]);
                        let w = cell.chars().count();
                        (cell, w)
                    }
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..columns.len())
        .map(|i| {
            let header = headers[i].1;
            let body = rows.iter().map(|r| r[i].1).max().unwrap_or(0);
            header.max(body)
        })
        .collect();
    let mut lines = vec![emit_row(&headers, &widths, color, |_| Some("1"))];
    lines.extend(
        rows.iter()
            .map(|r| emit_row(r, &widths, color, |i| column_sgr(columns[i].as_str()))),
    );
    lines.join("\n")
}

/// Render one task as a vertical record — a `field: value` line per column,
/// values **untruncated**, and a multi-line value continued under its label.
/// This is `show`'s human view: a single task across the aligned table
/// degenerates into one unreadable row, especially for long fields like `notes`;
/// the record reads like `git show`. JSON/JSONL output is unaffected.
pub(crate) fn render_record(
    task: &TaskState,
    columns: &[String],
    color: bool,
    blockers: &BTreeSet<String>,
) -> String {
    let label_w = columns.iter().map(String::len).max().unwrap_or(0) + 1; // +1 for ':'
    let indent = " ".repeat(label_w + 1);
    let mut lines = Vec::new();
    for col in columns {
        let label = sgr(&format!("{:<label_w$}", format!("{col}:")), "1", color);
        // The value's display lines: deps puts one type group per line, each
        // styled by its kind; anything else is the human cell continued across
        // its own newlines, the first line carrying the column color.
        let parts: Vec<String> = if col == "deps" {
            deps_groups(task)
                .into_iter()
                .map(|(rel, text)| sgr(&text, group_sgr(blockers.contains(&rel)), color))
                .collect()
        } else {
            let value = human_cell(task, col);
            let mut parts: Vec<String> = value.split('\n').map(str::to_string).collect();
            if let Some(code) = column_sgr(col) {
                if color && parts.first().is_some_and(|f| !f.is_empty()) {
                    parts[0] = sgr(&parts[0], code, true);
                }
            }
            parts
        };
        let first = parts.first().map_or("", String::as_str);
        lines.push(format!("{label} {first}").trim_end().to_string());
        for cont in parts.iter().skip(1) {
            lines.push(format!("{indent}{cont}").trim_end().to_string());
        }
    }
    lines.join("\n")
}

/// Pad each `(cell, plain_width)` to its column width — padding by the plain
/// width, so an SGR-laden cell (deps) still aligns — and, when `color`, wrap it
/// in the SGR code `code_for(i)` returns. Cells are joined with two spaces and
/// the trailing padding is trimmed.
fn emit_row(
    cells: &[(String, usize)],
    widths: &[usize],
    color: bool,
    code_for: impl Fn(usize) -> Option<&'static str>,
) -> String {
    cells
        .iter()
        .zip(widths)
        .enumerate()
        .map(|(i, ((c, plain), w))| {
            let padded = format!("{c}{}", " ".repeat(w.saturating_sub(*plain)));
            match code_for(i) {
                Some(code) if color => sgr(&padded, code, true),
                _ => padded,
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

/// Pretty JSON array: one indented object per line, wrapped in `[ ]`.
fn render_json(tasks: &[&TaskState], columns: &[String]) -> String {
    if tasks.is_empty() {
        return "[]".to_string();
    }
    let objects: Vec<String> = tasks
        .iter()
        .map(|t| format!("  {}", json_object(t, columns)))
        .collect();
    format!("[\n{}\n]", objects.join(",\n"))
}

/// Newline-delimited JSON (NDJSON): one compact object per line, no array
/// wrapper — better for streaming, `grep`, and agents. Empty input yields no
/// lines (an empty string).
fn render_jsonl(tasks: &[&TaskState], columns: &[String]) -> String {
    tasks
        .iter()
        .map(|t| json_object(t, columns))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One task as a compact JSON object over `columns`, in order. A column the task
/// lacks is OMITTED rather than emitted as null; only the built-ins `id`/`deps`
/// always resolve (`deps` is `{}` when empty, which is data, not absence).
fn json_object(task: &TaskState, columns: &[String]) -> String {
    let pairs: Vec<String> = columns
        .iter()
        .filter_map(|c| {
            cell_value(task, c).map(|v| {
                let key = serde_json::to_string(c).unwrap_or_default();
                format!("{key}:{}", serde_json::to_string(&v).unwrap_or_default())
            })
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}

/// A column's value for the human table: `deps` as its plain type groups joined
/// by `"; "` (`depends_on: db, web; relates_to: x`), a bare string, an array
/// joined by `", "` (so any list field reads like a deps group), or compact JSON
/// for anything else. Empty for a column the task lacks.
fn human_cell(task: &TaskState, col: &str) -> String {
    if col == "deps" {
        return deps_groups(task)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("; ");
    }
    cell_value(task, col)
        .as_ref()
        .map(human_display)
        .unwrap_or_default()
}

/// Render a single JSON value for the human table (see [`human_cell`]).
fn human_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(human_display)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Truncate `s` to `max_width` characters (0 = no limit), with a trailing `…`
/// when cut. Shared by the human table and `dep tree`'s title column.
pub(crate) fn truncate(s: &str, max_width: usize) -> String {
    if max_width == 0 || s.chars().count() <= max_width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // unwrap is the conventional assertion style in tests
mod tests {
    use super::*;
    use crate::test_support::{display, task};
    use std::collections::BTreeMap;

    /// The default readiness-gating type set the human renderers style by.
    fn blockers() -> BTreeSet<String> {
        BTreeSet::from(["depends_on".to_string()])
    }

    /// A task with both a gating (`depends_on`) and an info (`relates_to`) edge.
    fn mixed_edges_task() -> TaskState {
        let mut t = task("api", &["db", "web"], &[]);
        t.relationships
            .insert("relates_to".to_string(), vec!["x".to_string()]);
        t
    }

    #[test]
    fn full_columns_keeps_present_configured_then_alphabetical() {
        // Default config columns are id,title,status,deps; this task has no title,
        // so it is dropped, and the extra `priority` sorts after the configured
        // tail. JSON over the same columns carries the present fields.
        let t = task(
            "api",
            &[],
            &[
                ("status", serde_json::json!("open")),
                ("priority", serde_json::json!(3)),
            ],
        );
        let cols = full_columns(&[&t], &DisplayConfig::default());
        assert_eq!(
            cols,
            ["id", "status", "deps", "priority"],
            "canonical present-only set: {cols:?}"
        );
        let json = render_json(&[&t], &cols);
        assert!(json.contains(r#""status":"open""#), "json: {json}");
        assert!(json.contains(r#""priority":3"#), "json: {json}");
    }

    #[test]
    fn canonical_full_order_shared_by_human_and_json() {
        // Configured columns come first in their exact order; remaining fields
        // follow alphabetically. `deps` keeps its configured slot.
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "status".into(), "deps".into()],
            max_width: 0,
            column_max_width: BTreeMap::new(),
            sort: String::new(),
            list_layout: Layout::Table,
            show_layout: Layout::List,
        };
        let t = task(
            "api",
            &["db"],
            &[
                ("zeta", serde_json::json!(1)),
                ("status", serde_json::json!("open")),
                ("alpha", serde_json::json!(2)),
            ],
        );
        let cols = full_columns(&[&t], &cfg);
        assert_eq!(
            cols,
            ["id", "status", "deps", "alpha", "zeta"],
            "configured order then alphabetical extras: {cols:?}"
        );

        // The human header tokens are exactly the columns, in order.
        let full = display(OutputFormat::Human, true, None);
        let human = render_human(
            &[&t],
            &cols,
            &truncation_caps(&cols, &full, &cfg),
            false,
            &blockers(),
        );
        let header: Vec<String> = human
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = cols.iter().map(|c| c.to_uppercase()).collect();
        assert_eq!(header, expected, "human header follows canonical order");

        // The JSON keys appear in the identical order.
        let json = render_json(&[&t], &cols);
        let mut last = 0;
        for c in &cols {
            let at = json.find(&format!("\"{c}\"")).unwrap();
            assert!(at >= last, "json key `{c}` out of canonical order: {json}");
            last = at;
        }
    }

    #[test]
    fn sort_tasks_orders_by_column_missing_last_and_reverse() {
        let pri3 = task("a", &[], &[("priority", serde_json::json!(3))]);
        let pri1 = task("b", &[], &[("priority", serde_json::json!(1))]);
        let pri2 = task("c", &[], &[("priority", serde_json::json!(2))]);
        let none = task("d", &[], &[]); // no priority -> sorts last (ascending)
        let cfg = DisplayConfig::default();
        let args = |sort: &str, reverse: bool| DisplayArgs {
            output: OutputArgs {
                format: OutputFormat::Human,
                no_color: false,
            },
            full: false,
            columns: None,
            sort: Some(sort.to_string()),
            reverse,
            layout: None,
        };
        let ids =
            |tasks: &[&TaskState]| -> Vec<String> { tasks.iter().map(|t| t.id.clone()).collect() };

        // Numeric ascending, with the missing-value task last.
        let mut list = vec![&pri3, &pri1, &pri2, &none];
        sort_tasks(&mut list, &args("priority", false), &cfg);
        assert_eq!(ids(&list), ["b", "c", "a", "d"], "asc, missing last");

        // --reverse flips the whole order.
        let mut list = vec![&pri3, &pri1, &pri2, &none];
        sort_tasks(&mut list, &args("priority", true), &cfg);
        assert_eq!(ids(&list), ["d", "a", "c", "b"], "reversed");

        // An unknown column leaves only the id tiebreak (orders by id).
        let mut list = vec![&pri2, &pri3, &pri1];
        sort_tasks(&mut list, &args("nope", false), &cfg);
        assert_eq!(ids(&list), ["a", "b", "c"], "unknown column -> by id");
    }

    #[test]
    fn cell_value_unifies_columns_and_human_joins_arrays() {
        let mut t = task(
            "api",
            &["db", "web"],
            &[
                ("tags", serde_json::json!(["x", "y"])),
                ("priority", serde_json::json!(3)),
            ],
        );
        t.relationships
            .insert("relates_to".to_string(), vec!["infra".to_string()]);

        // cell_value is the single source of truth: id string, deps as the typed
        // relationships map, custom passthrough, and None for a missing field.
        assert_eq!(cell_value(&t, "id"), Some(serde_json::json!("api")));
        assert_eq!(
            cell_value(&t, "deps"),
            Some(serde_json::json!({"depends_on": ["db", "web"], "relates_to": ["infra"]}))
        );
        assert_eq!(cell_value(&t, "priority"), Some(serde_json::json!(3)));
        assert_eq!(cell_value(&t, "missing"), None);

        // Human cells: bare string, deps as labeled type groups, arrays joined,
        // numbers as their text, empty for a missing column.
        assert_eq!(human_cell(&t, "id"), "api");
        assert_eq!(
            human_cell(&t, "deps"),
            "depends_on: db, web; relates_to: infra"
        );
        assert_eq!(human_cell(&t, "tags"), "x, y", "custom arrays join");
        assert_eq!(human_cell(&t, "priority"), "3");
        assert_eq!(human_cell(&t, "missing"), "");
    }

    #[test]
    fn deps_cell_styles_groups_by_kind_and_truncates_plainly() {
        let t = mixed_edges_task();

        // Plain, uncapped: groups joined by `; `, width = visible chars.
        let plain = "depends_on: db, web; relates_to: x";
        let (cell, w) = deps_cell(&t, &blockers(), 0, false);
        assert_eq!(cell, plain);
        assert_eq!(w, plain.chars().count());

        // Colored: the gating group is bold, the info group dim, width unchanged.
        let (cell, w) = deps_cell(&t, &blockers(), 0, true);
        assert_eq!(
            cell,
            "\x1b[1mdepends_on: db, web\x1b[0m; \x1b[2mrelates_to: x\x1b[0m"
        );
        assert_eq!(w, plain.chars().count(), "width counts only visible chars");

        // Truncation cuts on the PLAIN text (`truncate` semantics: cap-1 + `…`),
        // never mid-escape; the cut group keeps its styling.
        let (cell, w) = deps_cell(&t, &blockers(), 10, false);
        assert_eq!(cell, "depends_o…");
        assert_eq!(w, 10);
        let (cell, w) = deps_cell(&t, &blockers(), 25, true);
        assert_eq!(
            cell,
            "\x1b[1mdepends_on: db, web\x1b[0m; \x1b[2mrel\x1b[0m…"
        );
        assert_eq!(w, 25);

        // No edges at all: empty cell, zero width.
        assert_eq!(
            deps_cell(&task("t", &[], &[]), &blockers(), 0, true),
            (String::new(), 0)
        );
    }

    #[test]
    fn record_view_puts_one_deps_type_group_per_line() {
        let t = mixed_edges_task();
        let cols = vec!["id".to_string(), "deps".to_string()];
        let out = render_record(&t, &cols, false, &blockers());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "deps: depends_on: db, web", "first group: {out}");
        assert_eq!(
            lines[2].trim(),
            "relates_to: x",
            "next group continues indented: {out}"
        );
        // Colored: bold gating group on the label line, dim info continuation.
        let colored = render_record(&t, &cols, true, &blockers());
        assert!(
            colored.contains("\x1b[1mdepends_on: db, web\x1b[0m"),
            "bold group: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[2mrelates_to: x\x1b[0m"),
            "dim group: {colored:?}"
        );
    }

    #[test]
    fn cmp_json_orders_numbers_strings_and_mixed_types() {
        use serde_json::json;
        assert_eq!(
            cmp_json(&json!(2), &json!(10)),
            Ordering::Less,
            "numeric, not lexical"
        );
        assert_eq!(cmp_json(&json!("a"), &json!("b")), Ordering::Less);
        // Mixed types fall back to a stable per-type rank (number < string).
        assert_eq!(cmp_json(&json!(1), &json!("1")), Ordering::Less);
    }

    #[test]
    fn human_has_header_and_unquoted_values() {
        let t = task("api", &["db"], &[("status", serde_json::json!("open"))]);
        let d = display(OutputFormat::Human, false, Some(&["id", "status", "deps"]));
        let out = render(&[&t], &d, &DisplayConfig::default(), &blockers(), "(none)");
        assert!(
            out.contains("ID") && out.contains("STATUS"),
            "header: {out}"
        );
        assert!(out.lines().any(|l| l.starts_with("api")), "row: {out}");
        // value is bare `open`, not JSON-quoted, and deps are labeled by type.
        assert!(
            out.contains("open") && !out.contains("\"open\""),
            "unquoted: {out}"
        );
        assert!(out.contains("depends_on: db"), "deps: {out}");
    }

    #[test]
    fn color_wraps_human_output_only_when_enabled() {
        let mut t = task("api", &["db"], &[("status", serde_json::json!("open"))]);
        t.relationships
            .insert("relates_to".to_string(), vec!["x".to_string()]);
        let cols = vec!["id".to_string(), "status".to_string(), "deps".to_string()];
        let caps = [0, 0, 0];

        // color=true: id cyan (36), status green (32), headers + gating deps
        // groups bold (1), info groups dim (2), reset.
        let colored = render_human(&[&t], &cols, &caps, true, &blockers());
        assert!(colored.contains("\x1b[36m"), "id cyan: {colored:?}");
        assert!(colored.contains("\x1b[32m"), "status green: {colored:?}");
        assert!(
            colored.contains("\x1b[1mdepends_on: db\x1b[0m"),
            "gating deps group bold: {colored:?}"
        );
        assert!(
            colored.contains("\x1b[2mrelates_to: x\x1b[0m"),
            "info deps group dim: {colored:?}"
        );
        assert!(colored.contains("\x1b[0m"), "reset: {colored:?}");
        // The values themselves survive (color only wraps, never replaces).
        assert!(colored.contains("api") && colored.contains("open"));

        // color=false: not a single escape byte.
        let plain = render_human(&[&t], &cols, &caps, false, &blockers());
        assert!(!plain.contains('\x1b'), "no escapes when off: {plain:?}");

        // The record view colors too, and stays clean when off.
        assert!(render_record(&t, &cols, true, &blockers()).contains('\x1b'));
        assert!(!render_record(&t, &cols, false, &blockers()).contains('\x1b'));

        // JSON is never colored, even via the shared render path.
        let json = render(
            &[&t],
            &display(OutputFormat::Json, false, Some(&["id", "status"])),
            &DisplayConfig::default(),
            &blockers(),
            "(none)",
        );
        assert!(!json.contains('\x1b'), "json never colored: {json:?}");
    }

    #[test]
    fn json_is_array_in_column_order() {
        let item = task(
            "api",
            &[],
            &[
                ("status", serde_json::json!("open")),
                ("priority", serde_json::json!(3)),
            ],
        );
        let args = display(
            OutputFormat::Json,
            false,
            Some(&["id", "priority", "status"]),
        );
        let out = render(
            &[&item],
            &args,
            &DisplayConfig::default(),
            &blockers(),
            "(none)",
        );
        assert!(out.trim_start().starts_with('['), "array: {out}");
        let id_at = out.find("\"id\"").unwrap();
        let pri_at = out.find("\"priority\"").unwrap();
        let status_at = out.find("\"status\"").unwrap();
        assert!(
            id_at < pri_at && pri_at < status_at,
            "keys follow column order: {out}"
        );
        assert!(
            out.contains("\"priority\":3"),
            "number stays a number: {out}"
        );
    }

    #[test]
    fn all_unions_fields_but_each_object_omits_absent_ones() {
        let a = task("a", &[], &[("x", serde_json::json!(1))]);
        let b = task("b", &[], &[("y", serde_json::json!(2))]);
        let d = display(OutputFormat::Json, true, None);
        let out = render(
            &[&a, &b],
            &d,
            &DisplayConfig::default(),
            &blockers(),
            "(none)",
        );
        // --full unions the column set: both x and y appear across the array.
        assert!(
            out.contains("\"x\"") && out.contains("\"y\""),
            "union: {out}"
        );
        // But an absent field is OMITTED, never emitted as null — no nulls anywhere.
        assert!(
            !out.contains("null"),
            "absent fields omitted, not null: {out}"
        );

        let empty = render(&[], &d, &DisplayConfig::default(), &blockers(), "(none)");
        assert_eq!(empty, "[]", "empty json is []");
    }

    #[test]
    fn jsonl_is_one_object_per_line_omitting_absent_fields() {
        let a = task("a", &["d"], &[("x", serde_json::json!(1))]);
        let b = task("b", &[], &[("y", serde_json::json!(2))]);
        let d = display(OutputFormat::Jsonl, true, None);
        let out = render(
            &[&a, &b],
            &d,
            &DisplayConfig::default(),
            &blockers(),
            "(none)",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one object per line: {out}");
        // Each line is a standalone object (no array brackets), absent keys gone.
        for line in &lines {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object(), "each line is a JSON object: {line}");
        }
        assert!(
            lines[0].contains(r#""x":1"#) && !lines[0].contains("\"y\""),
            "a: {}",
            lines[0]
        );
        assert!(
            lines[1].contains(r#""y":2"#) && !lines[1].contains("\"x\""),
            "b: {}",
            lines[1]
        );
        // deps is a built-in: always present as the typed map, {} when empty
        // (data, not absence).
        assert!(
            lines[0].contains(r#""deps":{"depends_on":["d"]}"#)
                && lines[1].contains(r#""deps":{}"#)
        );

        // Empty input yields no lines.
        assert_eq!(
            render(&[], &d, &DisplayConfig::default(), &blockers(), "(none)"),
            ""
        );
    }

    #[test]
    fn truncate_caps_long_values() {
        assert_eq!(truncate("hello", 0), "hello");
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn full_disables_truncation_but_default_and_columns_still_truncate() {
        let long = "a value that is definitely longer than the configured max width";
        let t = task("api", &[], &[("notes", serde_json::json!(long))]);
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "notes".into()],
            max_width: 20,
            column_max_width: BTreeMap::new(),
            sort: String::new(),
            list_layout: Layout::Table,
            show_layout: Layout::List,
        };

        // --full: the full value survives, no ellipsis.
        let full = render(
            &[&t],
            &display(OutputFormat::Human, true, None),
            &cfg,
            &blockers(),
            "(none)",
        );
        assert!(full.contains(long), "--full prints untruncated: {full}");
        assert!(!full.contains('…'), "--full adds no ellipsis: {full}");

        // Default (config columns) still truncates per max_width.
        let default = render(
            &[&t],
            &display(OutputFormat::Human, false, None),
            &cfg,
            &blockers(),
            "(none)",
        );
        assert!(!default.contains(long), "default truncates: {default}");
        assert!(default.contains('…'), "default shows ellipsis: {default}");

        // An explicit --columns view also still truncates.
        let cols = render(
            &[&t],
            &display(OutputFormat::Human, false, Some(&["id", "notes"])),
            &cfg,
            &blockers(),
            "(none)",
        );
        assert!(cols.contains('…'), "--columns still truncates: {cols}");
    }

    #[test]
    fn per_column_max_width_overrides_the_global() {
        // `notes` gets a wide override (60); `summary` falls back to max_width (10).
        let long = "0123456789abcdefghij"; // 20 chars
        let t = task(
            "api",
            &[],
            &[
                ("notes", serde_json::json!(long)),
                ("summary", serde_json::json!(long)),
            ],
        );
        let cfg = DisplayConfig {
            columns: vec!["id".into(), "notes".into(), "summary".into()],
            max_width: 10,
            column_max_width: std::iter::once(("notes".to_string(), 60)).collect(),
            sort: String::new(),
            list_layout: Layout::Table,
            show_layout: Layout::List,
        };
        let out = render(
            &[&t],
            &display(OutputFormat::Human, false, None),
            &cfg,
            &blockers(),
            "(none)",
        );
        // notes keeps all 20 chars (override 60 > 20, no ellipsis); summary is cut.
        assert!(out.contains(long), "notes column not truncated: {out}");
        assert!(
            out.contains('…'),
            "summary column truncated to max_width: {out}"
        );

        // --full ignores the per-column map entirely: both survive intact.
        let full = render(
            &[&t],
            &display(OutputFormat::Human, true, None),
            &cfg,
            &blockers(),
            "(none)",
        );
        assert!(!full.contains('…'), "--full disables truncation: {full}");
    }

    #[test]
    fn record_view_is_vertical_untruncated_and_keeps_multiline() {
        let long = "a value that is definitely much longer than any default max width";
        let t = task(
            "api",
            &["db"],
            &[
                ("status", serde_json::json!("open")),
                ("notes", serde_json::json!("line one\nline two")),
                ("blurb", serde_json::json!(long)),
            ],
        );
        let cols = full_columns(&[&t], &DisplayConfig::default());
        let out = render_record(&t, &cols, false, &blockers());

        // One field per line: `id` value is `api`, status its own line.
        assert!(
            out.lines()
                .any(|l| l.starts_with("id:") && l.split_whitespace().last() == Some("api")),
            "vertical id line: {out}"
        );
        assert!(
            out.lines()
                .any(|l| l.starts_with("status:") && l.ends_with("open")),
            "status line: {out}"
        );
        // Long values are never truncated (the whole point of the record view).
        assert!(
            out.contains(long) && !out.contains('…'),
            "untruncated: {out}"
        );
        // A multi-line value continues under its label, with no second `notes:`.
        assert!(out.contains("line one"), "first notes line: {out}");
        assert!(
            out.lines()
                .any(|l| l.trim() == "line two" && l.starts_with(' ')),
            "continuation line indented: {out}"
        );
    }
}
