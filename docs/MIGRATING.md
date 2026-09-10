# Migrating an existing backlog into taska

**This page is written for an AI agent to execute.** Point your agent at it with
the tracker you are leaving, and it will do the migration with your project's own
conventions in mind:

> Read docs/MIGRATING.md in the taska repo and migrate our issues from
> `<path-or-tool>` into a taska store in this repository.

There is deliberately no `ta import`. A mechanical importer has to guess at
things it cannot see - which of your status values means "done", whether a
`related` link should gate readiness, which fields are worth keeping - and
guessing badly is worse than not importing. An agent reading your repository can
answer all three. Where it cannot, it should ask you rather than invent an answer.

---

## 0. Ground yourself

If a store already exists, `ta prime` prints **that store's** schema, statuses and
relationship types - always read it first, because everything below is
configurable and another repo will differ. Otherwise create one:

```bash
ta init          # creates .taska/, registers the merge driver, commits both
```

`ta <command> --help` is authoritative for flags. Never hand-edit `.taska/*.jsonl`.

## 1. Read the source before deciding anything

Do not assume a format. Look at what is actually there:

- **beads**: `.beads/issues.jsonl` (or `bd export`) - one JSON object per line.
- **Markdown**: `TODO.md`, `PLAN.md`, checklists scattered through docs.
- **GitHub**: `gh issue list --state all --json number,title,body,state,labels,assignees`.
- Anything else: a CSV export, a Jira dump, a wiki page.

Then answer these, from the data rather than from habit:

1. Which fields are **actually populated**? Ignore ones that are always empty -
   migrating an always-null `estimate` field just adds a column nobody fills.
2. Which values does the status field take, and **which of them means done**?
3. Which links are real **blockers** (work cannot start) versus merely related?
4. Are there parent/child relationships, or is the graph flat?

## 2. Choose ids you will still recognise

taska ids are free-form strings, not numbers, and they are what you will type for
the rest of the project's life. Prefer short, self-describing, kebab-case ids
(`migrate-db`, `deploy-api`) over carrying `bd-a1f3` across.

Keep the old id in a field so existing links, commit messages and PR bodies still
resolve:

```bash
ta create migrate-db title="Migrate the database" legacy_id=bd-a1f3 --new-field
```

## 3. Decide the schema up front - the whole migration hinges on this

**Recommended: declare a schema first.** You already know every field the export
uses, so declare them once in `.taska/config.toml` and every later write is
checked rather than merely accepted:

```toml
[task_types.task]
fields = {
  title     = { type = "string", required = true },
  notes     = "string",
  owner     = "string",
  legacy_id = "string",
  priority  = { type = "uint", min = 1, max = 3 },
  status    = { type = "enum", values = ["open", "in_progress", "closed"] },
}
```

Then **align the workflow with those values** - this is the step that bites:

```bash
ta config set workflow.default_status open     # stamped on create; MUST be in the enum
ta config set workflow.done_status closed      # what `--ready` treats as satisfied
ta config validate
```

> If `default_status` is not one of the enum's values, **every** `ta create` that
> omits a status fails schema validation. The shipped default is `todo`; if your
> source uses `open`/`in_progress`/`closed`, either change it as above or add
> `todo` to the enum. `done_status` must be in the enum too, or nothing can ever
> be completed.

**Alternative: no schema.** Leave `[task_types]` undeclared and the store stays
schema-agnostic - useful for a quick evaluation. Two rules then apply:

- The **first** task seeds the field vocabulary. Any *later* task introducing a
  new field name is rejected until you pass `--new-field` once.
- **An empty value does not register a field name.** `notes=""` means *unset*, so
  it stores nothing and `notes` stays unknown - the next task using it is still
  rejected. Seed a field with a real value, or declare it in the schema.

## 4. Create every task before you create any edge

`ta dep add` rejects an edge whose target does not exist yet, so do all the tasks
first, in any order, then all the edges.

```bash
ta create migrate-db type=task title="Migrate the database" status=open priority=2 owner=alice
```

For descriptions, comments and anything multi-line, read from stdin or a file
instead of fighting argv quoting:

```bash
jq -r .description issue.json | ta update migrate-db notes=@-
ta update migrate-db notes=@/tmp/issue-body.md
```

Preserve the source's own history in the notes rather than trying to fabricate
timestamps: `create_time`/`update_time`/`close_time` are computed by taska from
the event log and cannot be set.

> **Do not drive the migration from a line-based shell loop.** Descriptions
> contain newlines, so `while read` over a flattened export silently splits one
> issue into several and creates phantom tasks named after a fragment of someone's
> comment. It looks like it worked - the count is just wrong. Iterate in a real
> language and pass bodies over stdin, as below.

A complete driver, run against a beads-style export:

```python
import json, subprocess
SLUG = {"bd-a1f3": "migrate-db", "bd-b2c4": "deploy-api"}   # your chosen ids
issues = [json.loads(l) for l in open(".beads/issues.jsonl")]

for i in issues:                                  # every task FIRST
    subprocess.run(["ta", "create", SLUG[i["id"]], "type=task",
                    f"title={i['title']}", f"status={i['status']}",
                    f"priority={i['priority']}", f"owner={i['assignee']}",
                    f"legacy_id={i['id']}"], check=True)
    if i["description"]:                          # multi-line body over stdin
        subprocess.run(["ta", "update", SLUG[i["id"]], "notes=@-"],
                       input=i["description"].encode(), check=True)

for i in issues:                                  # then every edge
    for d in i["dependencies"]:
        if d["type"] == "blocks":                 # "X blocks Y" -> Y depends_on X
            subprocess.run(["ta", "dep", "add", SLUG[d["target"]],
                            f"depends_on={SLUG[i['id']]}"], check=True)
```

`check=True` matters: a schema violation exits `2` and you want the migration to
stop on the first bad record, not carry on writing half-mapped tasks.

## 5. Map the links onto declared relationship types

Every edge needs a type declared in `[relationships]`. The defaults are
`depends_on` (blocker, inverse `blocks`), `has_subtask` (hierarchy, inverse
`subtask_of`), `relates_to` and `duplicates` (both informational). Map by
**meaning, not by name**:

| Source link | taska type | Why |
|---|---|---|
| blocks / depends-on / blocked-by | `depends_on` | gates `--ready` and cycle checks |
| parent / epic / subtask | `has_subtask` | gates like a blocker, renders as a child |
| related / see-also / discovered-from | `relates_to` | informational; must NOT gate readiness |
| duplicate | `duplicates` | informational |

```bash
ta dep add deploy-api depends_on=migrate-db
```

Mind the direction: `ta dep add A depends_on=B` means **A waits for B**. If the
source says "A blocks B", the taska edge is `ta dep add B depends_on=A`. Getting
this backwards silently inverts your whole plan, so check one edge by hand with
`ta dep tree` before doing the rest.

## 6. Verify, and let the exit codes tell you

```bash
ta status                 # counts: total, per-status, blocked, ready, closed
ta dep cycles             # must report no cycles
ta list --ready           # sanity-check what it thinks is actionable
ta list --format json     # diff against the source, field by field
```

Compare the count against the source. Then confirm the **shape** is right, not
just the size: a migration where everything is `--ready` usually means the edges
did not land, and one where nothing is ready usually means they are inverted.

Writes exit `1` on a general error, `2` on a schema violation and `3` on a failed
`--if` precondition, so a migration script can branch on the kind of failure
rather than parsing messages.

Finally, commit the store with the code it describes:

```bash
git add .taska && git commit -m "Migrate backlog from <source> into taska"
```

## 7. Report what you could not map - do not guess

Finish by telling the human, explicitly:

- tasks whose status had no clear equivalent, and what you chose;
- links you dropped, or demoted to informational because you could not tell
  whether they gate;
- fields you left behind as always-empty or meaningless in the new store;
- anything where the source contradicted itself (an edge to a deleted issue, a
  parent cycle).

A migration that names its own gaps is finished. One that hides them is not.
