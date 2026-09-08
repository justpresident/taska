# docs/demo.tape - the README demo, recorded with charmbracelet/vhs.
#
#   go install github.com/charmbracelet/vhs@latest   # or: brew install vhs
#   cargo build --release
#   TA="$PWD/target/release/ta" PATH="$PWD/target/release:$PATH" vhs docs/demo.tape
#
# Produces docs/demo.gif (~45s). The story, in one take: dependencies gate what
# is ready, an agent branch and main both edit the same task, and the merge
# driver keeps BOTH edits without a human. Verified working against 1.2.0.

Output docs/demo.gif

Require ta

Set Shell bash
Set FontSize 16
Set Width 1280
Set Height 760
Set Padding 24
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

# --- setup, never shown -----------------------------------------------------
Hide
Type "bash docs/demo-setup.sh /tmp/taska-demo && cd /tmp/taska-demo && clear"
Enter
Sleep 3s
Show

# --- file two tasks and a dependency ----------------------------------------
Type "ta create migrate-db title='Run DB migration' status=open owner=alice"
Enter
Sleep 900ms

Type "ta create deploy-api title='Deploy the API' status=open owner=bob"
Enter
Sleep 900ms

Type "ta dep add deploy-api depends_on=migrate-db"
Enter
Sleep 1200ms

# --- readiness is computed from the dependency graph ------------------------
Type "ta list --ready"
Enter
Sleep 2500ms

Type "git add -A && git commit -qm 'track the backlog'"
Enter
Sleep 1s

# --- an agent works on its own branch ---------------------------------------
Type "# an agent picks up the migration on its own branch"
Enter
Sleep 800ms
Type "git checkout -q -b agent/migrate && ta update migrate-db status=closed"
Enter
Sleep 1500ms
Type "git commit -qam 'agent: migration done'"
Enter
Sleep 800ms

# --- meanwhile you edit the same task on main -------------------------------
Type "# meanwhile, on main, you reassign that same task"
Enter
Sleep 800ms
Type "git checkout -q main && ta update migrate-db owner=carol"
Enter
Sleep 1500ms
Type "git commit -qam 'main: reassign'"
Enter
Sleep 1200ms

# --- the merge: no database, no daemon, no human ----------------------------
Type "git merge agent/migrate -m 'merge agent branch'"
Enter
Sleep 2500ms

Type "# both edits survived - status from the agent, owner from you"
Enter
Sleep 800ms
Type "ta show migrate-db"
Enter
Sleep 3500ms

Type "# and closing the migration unblocked the deploy"
Enter
Sleep 800ms
Type "ta list --ready"
Enter
Sleep 4s
