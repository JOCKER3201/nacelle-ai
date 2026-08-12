# The supervisor: a local agent that watches, and a remote one that is asked

This is the design for how the two backends relate to each other. It is not a
menu of options — it is the contract the code has to satisfy, and the reasons
each rule exists. Read it before changing anything under `src/supervise/`,
`src/redact/`, or the approval path.

## The shape

One agent runs continuously on the local machine (Ollama) for as long as the
desktop is running. It observes, answers, and acts within its limits. When it
judges a task beyond itself, it *asks* — it escalates to Claude over the
network. Claude is never the first responder and never runs unasked.

Two properties follow from that shape, and both are load-bearing:

- **The local agent is the only one with eyes.** It can read anything the user
  can read without `sudo`. Claude sees only what the local agent decides to
  send, after that content has passed redaction.
- **Neither agent can change anything on its own.** Reading is theirs. Writing
  and deleting belong to the user, and every such act requires an explicit,
  per-action authorization. There is no "allow all", no session-wide grant, and
  no default-yes.

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

### Layer 2 — never send it

Everything that survives layer 1 and is *about to cross the network* is scanned
deterministically. Pattern matching, not judgment:

- PEM armor (`-----BEGIN ... PRIVATE KEY-----`)
- provider-shaped credentials: `sk-ant-…`, `sk-…`, `ghp_…`, `gho_…`,
  `AKIA…`, `xox[baprs]-…`, `AIza…`
- bearer tokens and `Authorization:` header lines
- JWTs (three base64url segments separated by dots)
- connection strings carrying a password (`scheme://user:pass@host`)
- long high-entropy strings that match no natural-language profile

A hit does not warn — it **redacts**, replacing the value with a marker that
says what was removed and why. The agent's own reasoning sees the marker, so it
knows something was withheld and can ask the user rather than silently
producing a wrong answer.

### Layer 3 — then ask the model

Only now does the local model read the redacted payload and give an opinion on
whether it still looks sensitive in ways patterns can't catch: a person's
medical detail, an unreleased business plan, a private message. This layer is
genuinely useful — it catches meaning, which regexes cannot — but it runs
*after* the two layers that catch structure, and it can only ever remove more,
never restore what layers 1 and 2 took out.

### Layer 4 — the user sees the manifest

Before the first escalation of a session, and whenever the outgoing payload
includes file content the user has not already seen in the conversation, the
user is shown **what is about to leave the machine**: which files, how many
bytes, and what was redacted. Escalation proceeds on their word.

This is the layer that makes the other three auditable. Without it the user has
to trust three mechanisms they cannot observe.

## Escalation: when the local agent asks for help

The decision to escalate must not rest solely on the local model saying "I
can't do this" — small models are unreliable narrators of their own competence
in both directions. They give up on things they could do, and confidently
mangle things they cannot.

So escalation has deterministic triggers alongside the model's own judgment:

- the user asks for it explicitly ("ask Claude") — always honored, always available
- the local model failed the same task twice, or its tool calls errored repeatedly
- the task exceeds the local model's context window
- the task needs a capability the local model doesn't have (no tool support in
  the loaded model, for instance)
- the local model asks to escalate and states a reason the user can read

And escalation is refusable: the user can pin a session local-only, and in that
mode the agent says what it cannot do rather than reaching for the network.
A machine with no token, or with no network, degrades to exactly this — the
local half must never depend on the remote half being reachable.

## Watching in the background without burning the machine

"Monitors the whole system" cannot mean a 30B model in a spin loop. Continuous
inference would make the desktop unusable and tell us nothing new most of the
time.

The supervisor is **event-driven**: it wakes on something happening — a widget
reporting an anomaly, a threshold crossed in the telemetry the desktop already
collects, a user question — and sleeps otherwise. Cheap deterministic checks
run continuously and are not the model's job; the model is invoked when a check
fires and interpretation is actually needed.

It must be pausable and stoppable from the interface, and it must say plainly
when it is running, because a background process that reads the user's files
and cannot be seen or stopped is not a feature.

## The ban on writing, stated precisely

- Read tools and write tools are **separate sets**, not one set with a flag.
  A write tool cannot be reached through the read path.
- Every write or delete requires an authorization for **that specific action**,
  showing the exact change (path, before, after) before it is granted.
- There is no session-wide grant and no remembered answer. The cost of asking
  each time is small; the cost of a forgotten blanket grant is not.
- Deletion is the narrowest case: the agent proposes, and the user performs or
  explicitly authorizes. Nothing is removed on the agent's initiative.
- A refused authorization returns to the model as a tool result stating the
  user declined, so it can change plan instead of retrying blindly.
