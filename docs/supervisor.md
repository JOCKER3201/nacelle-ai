# The supervisor: a local agent that answers, and a remote one that is asked

This is the contract for how the two backends relate to each other, and the
reason each rule exists. Read it before changing anything under
`src/supervise/`, `src/redact/`, or the approval path.

**What this page is, as of 2026-08-18.** It was written as a design before
`nacelle-ai` was a daemon, and it described a program that does not exist: an
agent running continuously beside the desktop, with eyes on everything the user
can read, watching in the background. The daemon does none of that, and the
code says so in as many words (`crates/nacelle-ai/src/lib.rs`: *no timers, no
watchers*). Rather than stamping the whole page as history — most of it is the
contract the code satisfies today, and the confidentiality line is due its own
pass — it has been corrected in place, and every part that is still only a
design is marked:

> **NOT BUILT (design).** What was decided, and the file that proves nothing
> builds it.

Take a marked paragraph as a decision already made about what the thing should
be, not as a description of what runs. Take everything unmarked as something
you can go and read in the source.

## The shape

`nacelle-ai` is a **daemon**. It listens on a Unix socket and does nothing at
all until a command arrives on it — that is the owner's first rule and it is
measured by a test that holds a live daemon open and watches for a byte
(`crates/nacelle-ai/tests/daemon_idle.rs`). There is no process here that
observes, and none that wakes up on its own.

What runs is a session per connection, built the first time an `ask` needs one:

- **`auto`** — the local model (Ollama) with the nacelle configuration tools.
  Managing the interface, which is what the local model is for.
- **`local`** — chat the client pinned to this machine, explicitly, per command.
- **`claude`** — the remote model, asked because a command asked for it, behind
  the whole line below.

Two properties hold, and both are load-bearing:

- **The local agent is the only one that reads anything.** Claude sees only what
  is sent to it, after that content has passed the layers. What the local agent
  can read today is the **nacelle installation** — the themes, the layauts, the
  addons and the configuration file — through eight named tools and nothing
  else. There is no general file-reading tool in this program.

  > **NOT BUILT (design).** "It can read anything the user can read without
  > `sudo`" was the original shape, and it is why layer 1 exists in the form it
  > does. Today the denylist guards the reads the toolbox makes
  > (`src/tools/catalog.rs`, `src/tools/conf.rs`); a general read tool would be
  > the thing it was really written for, and it does not exist yet.

- **Neither agent can change anything on its own.** Reading is theirs. Writing
  and deleting belong to the user, and every such act requires an explicit,
  per-action authorization. There is no "allow all", no session-wide grant, and
  no default-yes. On a daemon that authorization is a wire event: the change
  travels to the client as `ev:approval` and goes nowhere until `cmd:approve`
  comes back. An approval nobody answers — the widget died, the connection
  closed — is a refusal by construction.

## Why the model is not the safety layer

The obvious implementation — let the local model look at the text and decide
whether it is confidential — is the weakest possible design, and it is the one
this document exists to prevent.

A model's judgment is a *probability*, not a boundary. A 30B model asked "does
this contain secrets?" will be right most of the time, and the times it is
wrong are exactly the times that matter: an SSH private key pasted into a log
file, a token embedded in a URL query string, a password in a shell history
line. One miss is unrecoverable — the bytes are on someone else's server and
cannot be recalled.

So the model is the **last** layer, not the first. The layers, in order of
strength:

And one thing has to be said before the list, because the list reads like a
proof and is not one. Layers 2 and 3 are **pattern matching over arbitrary
text**, and that is a problem nobody closes by patching it. Three rounds of
adversaries have attacked this boundary; each round found holes the previous
one had not, and the third found the redaction marker quoting the key it had
just cut. Every one of those was measured on the bytes handed to the socket.
A fourth round will find a fourth thing.

That is not an argument for giving up on the layers — they turn a careless
paste into a redacted one, which is most of the real risk. It is an argument
against ever describing them as a seal. The guarantee in this design is layer
1 and the pin: what is never read cannot leave, and a daemon pinned to the
machine opens no connection off it. Everything below is risk reduction, and the
README's "What this does not protect" carries the current, measured list of what
gets through.

### Layer 1 — never read it

The strongest guarantee about a secret is that the program never opened the
file. A hard denylist is checked before any read, on the canonical path, and it
is not overridable by the model, by a prompt, or by a tool argument:

- `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gh`, `~/.config/anthropic`,
  `~/.local/share/keyrings`, and the equivalents for other credential stores
- password managers and their databases (`*.kdbx`, `pass` stores)
- browser profile directories (cookies, saved logins, session stores)
- `.env` and `.env.*` anywhere in the tree
- key and certificate material by extension (`*.pem`, `*.key`, `id_*`,
  `*.p12`, `*.pfx`) and by content sniff (PEM armor headers)
- shell history files

A read against any of these is refused with a said reason. The refusal is
reported to the user, not silently swallowed — a program that quietly declines
teaches the user nothing, and they may be asking for a legitimate reason.

The guard is a **parameter** to every function in the toolbox that opens a
file, so a read path that forgot it does not compile. That is what makes the
rule survive the day a general read tool is added: the new tool cannot be
written without passing it.

### Layer 2 — never send it

Everything that survives layer 1 and is *about to cross the network* is scanned
deterministically. Pattern matching, not judgment:

- PEM armor (`-----BEGIN ... PRIVATE KEY-----`)
- provider-shaped credentials: `sk-ant-…`, `sk-…`, `ghp_…`, `gho_…`,
  `glpat-…`, `AKIA…`, `xox[baprs]-…`, `xapp-…`, `AIza…`
- bearer tokens and `Authorization:` header lines
- JWTs (three base64url segments separated by dots)
- connection strings carrying a password (`scheme://user:pass@host`)
- long high-entropy strings that match no natural-language profile
- the same, read again with the payload's **wrapping taken out** — line
  breaks, spaces and invisible characters woven through a key — because a
  credential broken into pieces is not one run and every rule above would
  otherwise look at each piece and pass

A hit does not warn — it **redacts**, replacing the value with a marker that
says what was removed and why. The agent's own reasoning sees the marker, so it
knows something was withheld and can ask the user rather than silently
producing a wrong answer. The marker never quotes what it replaced: every word
in it is one of this program's own constants.

Two strings on the wire cannot carry a marker, because the far side verifies or
looks them up: the model id and a thinking block's signature. Those are read by
the same rules and a hit **stops the turn** instead. A tool call's identifier
is neither redacted nor checked — it is **renumbered**, so whatever the local
model put in that field is not what leaves.

What this layer cannot do is written out in the README under *What this does not
protect*. The two that matter most: a short secret with no name and no prefix is
indistinguishable from an identifier and goes; and a forty-character hex digest
is indistinguishable from a forty-character token, so both are cut.

### Layer 3 — then ask the model

Only now does the local model read the redacted payload and give an opinion on
whether it still looks sensitive in ways patterns can't catch: a person's
medical detail, an unreleased business plan, a private message. This layer is
genuinely useful — it catches meaning, which regexes cannot — but it runs
*after* the two layers that catch structure, and it can only ever remove more,
never restore what layers 1 and 2 took out.

It runs only where there is a local model on the **loopback interface** to run
it: asking a model on another machine whether a payload may be sent has already
sent it, and `LocalReviewer::new` refuses to be built on anything else. A
machine without one has a weaker line, not a broken one — the pattern rules
still run, and the manifest says which of the two happened. An `ollama_host`
named in `nacelle-ai.ron` decides *which* host is asked; it cannot decide
whether that rule applies.

### Layer 4 — the user sees the manifest

Before the first escalation of a session, and whenever the outgoing payload
includes file content the user has not already answered for, the user is shown
**what is about to leave the machine**: which files, how many bytes, and what
was redacted. Escalation proceeds on their word.

On the daemon that showing is a wire event. The manifest is rendered and sent as
`ev:approval` on the connection that asked, and nothing goes until `cmd:approve`
answers with the same id — per action, every time. The protocol has no way to
say "allow all" and the daemon does not invent one. A client that hangs up with
the question open has refused it.

A request records no provenance of its own, so "which files" is worked out from
the calls the results answer: a result whose call passed a path to a declared
tool is a file with a name. A file the model quoted into its own prose has no
name, and the manifest says so on its own face rather than letting the list read
as complete — which also means the second rule above fires on files and not on
prose. Something private the user typed is disclosed on the first escalation of
the session and not again.

The size on the manifest covers every string that reaches the socket, including
the two the layers may read but not edit.

This is the layer that makes the other three auditable. Without it the user has
to trust three mechanisms they cannot observe.

## Escalation: when the local agent asks for help

The decision to escalate must not rest solely on the local model saying "I
can't do this" — small models are unreliable narrators of their own competence
in both directions. They give up on things they could do, and confidently
mangle things they cannot.

So escalation has deterministic triggers alongside the model's own judgment:

- the user asks for it explicitly ("ask Claude") — always honored, always
  available, and on the daemon it is a command that names its backend
- the local model failed the same task twice, or its tool calls errored
  repeatedly
- the task exceeds the local model's context window
- the task needs a capability the local model doesn't have (no tool support in
  the loaded model, for instance)
- the local model asks to escalate and states a reason the user can read

Today the daemon reaches this path by the first of those: a client asks for
`claude` by name, and the trigger recorded in the manifest is that ask. The
other four are built in the core (`src/supervise/escalate.rs`) and are what a
session that begins on the local model and runs out of road would use.

> **NOT BUILT (design).** Nothing in the daemon escalates a running `local`
> session to `claude` by itself. The two are separate sessions, cached by name
> in `src/serve.rs`, and a client that wants the remote model says so in a
> command. An automatic hand-over would be a new decision about consent, not a
> refactor.

And escalation is refusable: the user can pin a session local-only —
`--backend local`, or `backend: Named("local")` in `nacelle-ai.ron` — and in
that mode the agent says what it cannot do rather than reaching for the network.
A machine with no token, or with no network, degrades to exactly this — the
local half must never depend on the remote half being reachable.

## Watching in the background without burning the machine

> **NOT BUILT (design).** `Watch` exists in the core, complete and tested
> (`src/supervise/watch.rs`, `tests/supervise.rs`), and **nothing constructs
> one outside those tests**. The daemon leaves it unplugged deliberately: the
> owner's first rule is that nothing happens without a command, and a watcher
> is a thing that happens without one. What follows is the shape it must have
> if it is ever plugged in.

"Monitors the whole system" cannot mean a 30B model in a spin loop. Continuous
inference would make the desktop unusable and tell us nothing new most of the
time.

The supervisor is **event-driven**: it wakes on something happening — a widget
reporting an anomaly, a threshold crossed in the telemetry the desktop already
collects, a user question — and sleeps otherwise. Cheap deterministic checks
run continuously and are not the model's job; the model is invoked when a check
fires and interpretation is actually needed. Nothing in `watch.rs` calls a model
at all, which is what makes "the model sleeps unless something happened" a
property of the code rather than a promise in a comment.

It must be pausable and stoppable from the interface, and it must say plainly
when it is running, because a background process that reads the user's files
and cannot be seen or stopped is not a feature. `WatchHandle::pause`,
`WatchHandle::stop` and `Watch::describe` are there for exactly that, and there
is no way to start one without a handle to ask.

Plugging it in needs one thing this document cannot decide: how an observation
reaches a person. On the daemon there is no screen — the events would have to
travel over the socket to a widget, which means a protocol version that can
carry an event nobody asked for. That is a change to `proto` and to the four
clients, not a change here.

## The ban on writing, stated precisely

- Read tools and write tools are **separate sets**, not one set with a flag.
  A write tool cannot be reached through the read path.
- Every write or delete requires an authorization for **that specific action**,
  showing the exact change (path, before, after) before it is granted.
- There is no session-wide grant and no remembered answer. The cost of asking
  each time is small; the cost of a forgotten blanket grant is not.
- Deletion is the narrowest case: the agent proposes, and the user performs or
  explicitly authorizes. Nothing is removed on the agent's initiative. No tool
  in this program deletes anything today.
- A refused authorization returns to the model as a tool result stating the
  user declined, so it can change plan instead of retrying blindly.

## Where the tools may write at all

Not part of the original design and worth stating beside it, because it is the
narrowest fence and the one that holds first: the toolbox writes to exactly one
file, `nacelle-desktop.ron` in the user's own configuration directory. The path
is resolved canonically before every write, `..` collapsed and symlinks
followed, and anything landing outside that directory is refused rather than
written. The previous contents go to `<name>.bak` and the replacement is a
rename over the target, so an interrupted write leaves the old file or the new
one and never half of either.

The daemon's **own** configuration, `nacelle-ai.ron`, is read and never written
by anything here. A program that could rewrite the file that says where it may
listen and which model it may reach is a program whose settings are advice.
