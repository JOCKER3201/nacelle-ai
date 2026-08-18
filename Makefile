# nacelle-ai build system.
#
#   make install       — clean build and install to ~/.local/
#   sudo make install  — clean build and install to /usr/local/
#
# This installs ONE BINARY and nothing else. There is no window here,
# so there are no fonts, no icons and no .desktop entry: nobody starts
# this program from a menu.
#
# WHO STARTS THE DAEMON — the question this file had to answer before it
# could be written, and the answer is: THE DESKTOP DOES, on demand.
#
# nacelle-desktop already contains the whole mechanism (`spawn_ai_daemon`
# in its src/main.rs): before the board is built it connects to
# $XDG_RUNTIME_DIR/nacelle/ai.sock, and if nothing answers there it
# spawns `nacelle-ai` — the binary named by NACELLE_AI_BIN, or the bare
# name looked up on PATH. That lookup is the reason this file exists at
# all: with no installer there was no `nacelle-ai` on any PATH, so the
# spawn failed on every start and the four AI widgets of the upper board
# stood OFFLINE forever on an installed system. Nothing else was broken.
#
# So the whole job here is to put the binary where that lookup finds it,
# and to say so loudly when the chosen prefix is not on PATH.
#
# The alternatives were considered and rejected, in writing, because the
# next person will wonder:
#
#   * A systemd --user unit. Two starters race for one socket, and the
#     loser prints "another nacelle-ai is already listening" — on every
#     login of every machine that also runs the desktop. The daemon's
#     clients are the four widgets and nothing else, so a daemon started
#     at login on a machine with no nacelle desktop is a process nobody
#     can ever ask anything.
#   * Socket activation. The daemon places its own socket, sets 0700 on
#     the directory and 0600 on the file, and refuses to stand where a
#     live one answers. Being handed a descriptor instead would mean
#     giving all of that away to the unit file.
#   * An XDG autostart entry. The same race as the unit, with less
#     control over the environment.
#
# NO CONFIGURATION IS INSTALLED from here, deliberately and by the same
# rule nacelle-desktop's Makefile follows: one owner per file. The
# daemon reads nacelle-ai.ron where it finds one and runs on its own
# defaults where it does not, so there is nothing that has to be laid
# down for it to start. docs/nacelle-ai.ron.example is in the repository
# for somebody who wants to write one; it is documentation, not a file
# this installer places in /etc.
#
# Every install: removes the old build, builds, installs, removes the
# build. The prefix can be overridden: make install PREFIX=/opt/nacelle

ifeq ($(shell id -u),0)
PREFIX ?= /usr/local
else
PREFIX ?= $(HOME)/.local
endif

BINDIR = $(DESTDIR)$(PREFIX)/bin

.PHONY: all build install uninstall clean check

all: build

build:
	cargo build --release

# The gate this project runs before a commit: a build from nothing and
# the whole test suite. Named here so it is one word rather than folklore.
check:
	rm -rf target
	cargo build
	cargo test --workspace

install:
	rm -rf target
	cargo build --release
	install -Dm755 target/release/nacelle-ai "$(BINDIR)/nacelle-ai"
	@# The lookup this install exists to satisfy is a bare name on PATH.
	@# A prefix that is not on it installs a binary the desktop still
	@# cannot find, which is the failure this whole file is about — so
	@# it is said here rather than discovered as four OFFLINE widgets.
	@case ":$$PATH:" in \
		*":$(PREFIX)/bin:"*) ;; \
		*) echo ""; \
		   echo "NOTE: $(PREFIX)/bin is not on your PATH."; \
		   echo "      nacelle-desktop starts this daemon by the bare name"; \
		   echo "      \`nacelle-ai\`, so it will not find it. Either put that"; \
		   echo "      directory on PATH, or set NACELLE_AI_BIN to"; \
		   echo "      $(PREFIX)/bin/nacelle-ai in the session's environment."; \
		   echo "" ;; \
	esac
	@echo "no configuration installed — nacelle-ai runs on its own defaults"
	@echo "until a nacelle-ai.ron is written (see docs/nacelle-ai.ron.example)"
	rm -rf target
	@echo "nacelle-ai installed at $(BINDIR)/nacelle-ai"

# Removes what this file installed and nothing else. A configuration
# file is the user's, and no installer of this family deletes one.
uninstall:
	rm -f "$(BINDIR)/nacelle-ai"
	@echo "nacelle-ai removed from $(BINDIR)"
	@echo "nothing else was touched: a nacelle-ai.ron, if you wrote one, is yours"

clean:
	rm -rf target
