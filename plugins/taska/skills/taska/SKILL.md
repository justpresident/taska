---
name: taska
description: Use when working with tasks in a repo that has (or should have) a .taska/ directory - managing work with the `ta` CLI. This skill just bootstraps; `ta init` and `ta prime` carry the actual guidance.
---

# Working with taska

Two commands set you up; run them and follow what they print.

```bash
ta init     # create/sync the store, and write the ta cheat sheet + working
            # habits into this repo's AGENTS.md and CLAUDE.md
ta prime    # print THIS store's schema (fields, statuses, task and relationship
            # types) with copy-paste-ready examples in its own vocabulary
```

`ta init` is idempotent, so run it even when `.taska/` already exists to refresh
the guidance block. Then `ta <command> --help` is authoritative for any flags.
