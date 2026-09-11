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

# The merge demo doubles as the feed cut, so it is pinned to a feed-safe 80x24.
FLAGS_demo    := --cols 80 --rows 24
# Read-only against THIS repo's store: the tasks on screen are real ones.
FLAGS_backlog := --in-repo --cols 120 --rows 30
FLAGS_notes   := --in-repo --cols 110 --rows 34
FLAGS_fields  := --cols 115 --rows 24

.DEFAULT_GOAL := help
.PHONY: help all demo backlog notes fields social \
        preview-demo preview-backlog preview-notes preview-fields clean-casts

help:
	@printf 'Demo targets - run as `make -C docs <target>`\n\n'
	@printf '  %-16s %s\n' all      'every demo below'
	@printf '  %-16s %s\n' demo     'two branches edit one task, git merges both (80x24)'
	@printf '  %-16s %s\n' backlog  "taska's own backlog: status, open list, dep tree (120x30)"
	@printf '  %-16s %s\n' notes    'search an agent's notes, read the reasoning back (110x34)'
	@printf '  %-16s %s\n' fields   'your fields, one syntax, typo caught (115x24)'
	@printf '  %-16s %s\n' social   'feed-sized cut of the merge demo, re-rendered from its cast'
	@printf '\n'
	@printf '  %-16s %s\n' 'preview-<name>' 'watch that demo run without recording'
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

# Deliberately does NOT delete the GIFs: demo.gif is committed and the README
# links it.
clean-casts:
	rm -f $(DOCS)/*.cast
