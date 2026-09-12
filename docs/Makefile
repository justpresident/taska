# Regenerate the demo GIFs.
#
#   make -C docs                 # list the targets
#   make -C docs all             # every demo
#   make -C docs notes           # just that one
#   make -C docs preview-notes   # watch it run, record nothing
#   make -C docs -n all          # print the exact commands, run nothing
#
# Each demo's flags live in one variable below and are used by both its GIF and
# its preview, so the two cannot drift. The widths are not arbitrary - real task
# titles are long, and a wrapped TABLE is unreadable in a way wrapped prose is
# not; each .script header explains its own number.
#
# Needs: scriptty (with --cols and the \e escape), asciinema, agg. record-demo.sh
# checks for each and says which is missing.

DOCS := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
REC  := bash $(DOCS)/record-demo.sh

# WIDTH is per demo and deliberate: real task titles are long, and a wrapped
# TABLE is unreadable in a way wrapped prose is not. HEIGHT is not set here -
# record-demo.sh measures each session and fits the frame to it, so a short demo
# stops rendering into a tall, mostly-empty frame.
COLS_demo    := 80
COLS_backlog := 120
COLS_notes   := 110
COLS_fields  := 115

# The merge demo doubles as the feed cut, so its PTY is a feed-safe 80x24.
FLAGS_demo    := --cols $(COLS_demo) --rows 24
# Read-only against THIS repo's store: the tasks on screen are real ones.
FLAGS_backlog := --in-repo --cols $(COLS_backlog) --rows 30
FLAGS_notes   := --in-repo --cols $(COLS_notes) --rows 34
FLAGS_fields  := --cols $(COLS_fields) --rows 24

.DEFAULT_GOAL := help
.PHONY: help all demo backlog notes fields social \
        preview-demo preview-backlog preview-notes preview-fields \
        rerender rerender-demo rerender-backlog rerender-notes rerender-fields \
        clean-casts

help:
	@printf 'Demo targets - run as `make -C docs <target>`\n\n'
	@printf '  %-16s %s\n' all      'every demo below'
	@printf '  %-16s %s\n' demo     'two branches edit one task, git merges both (80x24)'
	@printf '  %-16s %s\n' backlog  "taska's own backlog: status, open list, dep tree (120x30)"
	@printf '  %-16s %s\n' notes    'search the notes an agent wrote, read the reasoning back (110x34)'
	@printf '  %-16s %s\n' fields   'your fields, one syntax, typo caught (115x24)'
	@printf '  %-16s %s\n' social   'feed-sized cut of the merge demo, re-rendered from its cast'
	@printf '\n'
	@printf '  %-16s %s\n' 'preview-<name>' 'watch that demo run without recording'
	@printf '  %-16s %s\n' rerender 'redraw every GIF from the casts on disk, without recording'
	@printf '  %-16s %s\n' 'rerender-<name>' 'redraw just that one'
	@printf '  %-16s %s\n' clean-casts 'remove the intermediate .cast files (keeps the GIFs)'

all: demo backlog notes fields

demo:    $(DOCS)/demo.gif
backlog: $(DOCS)/demo-backlog.gif
notes:   $(DOCS)/demo-notes.gif
fields:  $(DOCS)/demo-fields.gif
social:  $(DOCS)/demo-social.gif

$(DOCS)/demo.gif: $(DOCS)/demo.script $(DOCS)/demo-setup.sh
	$(REC) --script $< $(FLAGS_demo) --out $@

$(DOCS)/demo-backlog.gif: $(DOCS)/demo-backlog.script
	$(REC) --script $< $(FLAGS_backlog) --out $@

$(DOCS)/demo-notes.gif: $(DOCS)/demo-notes.script
	$(REC) --script $< $(FLAGS_notes) --out $@

$(DOCS)/demo-fields.gif: $(DOCS)/demo-fields.script $(DOCS)/demo-setup.sh
	$(REC) --script $< $(FLAGS_fields) --out $@

# Re-rendered from the merge demo's cast rather than recorded again: GIF frames
# are roughly one per typed character, and LinkedIn rejects anything over 400.
$(DOCS)/demo-social.gif: $(DOCS)/demo.cast
	$(REC) --from-cast $< --social --out $@

preview-demo:
	$(REC) --script $(DOCS)/demo.script $(FLAGS_demo) --preview

preview-backlog:
	$(REC) --script $(DOCS)/demo-backlog.script $(FLAGS_backlog) --preview

preview-notes:
	$(REC) --script $(DOCS)/demo-notes.script $(FLAGS_notes) --preview

preview-fields:
	$(REC) --script $(DOCS)/demo-fields.script $(FLAGS_fields) --preview

# Redraw from the casts already on disk. No recording, so this is quick and - for
# the two --in-repo demos - keeps the backlog they were captured with rather than
# picking up whatever is open today.
rerender: rerender-demo rerender-backlog rerender-notes rerender-fields

rerender-demo:
	$(REC) --from-cast $(DOCS)/demo.cast --cols $(COLS_demo) --out $(DOCS)/demo.gif

rerender-backlog:
	$(REC) --from-cast $(DOCS)/demo-backlog.cast --cols $(COLS_backlog) --out $(DOCS)/demo-backlog.gif

rerender-notes:
	$(REC) --from-cast $(DOCS)/demo-notes.cast --cols $(COLS_notes) --out $(DOCS)/demo-notes.gif

rerender-fields:
	$(REC) --from-cast $(DOCS)/demo-fields.cast --cols $(COLS_fields) --out $(DOCS)/demo-fields.gif

# Deliberately does NOT delete the GIFs: demo.gif is committed and the README
# links it.
clean-casts:
	rm -f $(DOCS)/*.cast
