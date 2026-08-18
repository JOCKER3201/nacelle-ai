# nacelle-ai

The AI agent for the [nacelle](https://github.com/JOCKER3201/nacelle-desktop)
environment — a **daemon** that can read the machine it runs on, use
tools, and answer in the desktop's own vocabulary.

> **THIS PROJECT IS WRITTEN WITH THE HELP OF AI (Anthropic's Claude
> models). Every line of code here was produced in collaboration with an
> AI assistant. This notice is deliberate and permanent — the project's
> owner requires that the authorship of this code is never left
> ambiguous.**

## Status: early

Very early. `nacelle-ai` is a daemon: it listens on a Unix socket,
answers a small JSON Lines protocol, and holds streamed conversations
with tools and an approval step. Nothing here is stable — the crate
layout, the contracts and the configuration format may all still change.

## A daemon, not a window

The window is gone — the owner's decision of 2026-08-16. What used to
be drawn in a window of its own is now four widgets in nacelle-addons
(media looping, photo work, file sorting, chat), and this program is
the process they all talk to. **There are no graphics here at all**: no
toolkit, no renderer, no winit — the whole dependency tree of the
window went with it.

Two rules govern the daemon, and both are the owner's:

* **Nothing without a command.** The daemon takes no action of its own
  initiative — no timers, no watchers, no autonomous work. The core's
  background `Watch` exists and stays unplugged. The accept loop waits,
  a connection waits, and the first byte of work happens because a
  command arrived on the socket. A test holds a live daemon open and
  measures that nothing happens.
* **The local model manages the interface, and nothing else.** Ollama
  runs in exactly two places: an `ask` whose backend is `auto` — the
  daemon's own agent, whose tools are the nacelle configuration tools —
  and an `ask` whose backend is `local`, chat the client pinned to this
  machine explicitly, per command. It is never handed the user's files
  to process: the media tools involve no model of any kind, and there
  is no path from a `tool` command to one.

Everything that is not about being a process lives in `nacelle-ai-core`.
The core draws nothing, knows nothing about windows, and knows nothing
about sockets either; the daemon crate is the door — a socket, a
protocol, and the policy above.

It also has no async runtime and never will: everything blocks, the
agent runs on a worker thread, and results travel over
`std::sync::mpsc` channels. A dependency that drags in a reactor cannot
be used here.

## The socket

```
$XDG_RUNTIME_DIR/nacelle/ai.sock      directory 0700, socket 0600
/tmp/nacelle-$UID/ai.sock             when XDG_RUNTIME_DIR is unset
```

The permissions are the boundary: everything crossing this socket is
the owner's conversation with their own machine, and they are set
unconditionally — even on a directory that already existed. A socket
file left behind by a killed daemon is swept and rebound; a socket a
*live* daemon answers on makes a second daemon refuse to start.

```
nacelle-ai [--backend auto|claude|local] [--model <id>]
           [--config <file>] [--print-config]
```

`--backend` sets what an `ask` that says `auto` resolves to. The
default leaves `auto` as the local model with the desktop's own
configuration tools. `claude` sends those asks to Claude instead —
behind the manifest, as always. `local` **pins** the daemon: nothing
goes off this machine, and a command asking for claude is refused with
the pin, not with a missing token. An ask that names its own backend
always wins over the flag. `--model` picks the model; unasked, the
backend's own default answers (the first model the local server
reports, or `claude-opus-4-8`). `--print-config` prints the settings in
force, the files they came from, and anything wrong with them, then
stops.

## Installing it, and who starts it

```sh
make install        # ~/.local/bin/nacelle-ai
sudo make install   # /usr/local/bin/nacelle-ai
```

One binary and nothing else: there is no window, so there are no fonts,
no icons and no menu entry. `make install` does a clean build (the old
`target` goes first, and again at the end), and warns if the prefix's
`bin` is not on your `PATH`.

**The desktop starts the daemon, and nothing else does.** Before it
builds the board, nacelle-desktop connects to
`$XDG_RUNTIME_DIR/nacelle/ai.sock`; if nothing answers there it spawns
`nacelle-ai` — the binary named by `NACELLE_AI_BIN`, or the bare name
looked up on `PATH`. That lookup is what this installer exists to
satisfy: with no installer there was no `nacelle-ai` on any `PATH`, so
on an installed system the spawn failed every time and the four AI
widgets of the upper board stood OFFLINE for good.

There is deliberately no systemd unit and no autostart entry. Two
starters race for one socket and the loser prints "another nacelle-ai is
already listening" on every login; and the daemon's only clients are the
four widgets, so one started at login on a machine without the desktop
is a process nobody can ask anything. The daemon is not killed when the
desktop exits — a restarted desktop finds it already answering.

## Its own configuration

```
$XDG_CONFIG_HOME/nacelle/nacelle-ai.ron      yours
$XDG_CONFIG_DIRS/nacelle/nacelle-ai.ron      the system's (/etc/xdg)
```

The FOLDER is the family and the FILE is the program: this sits beside
`nacelle-desktop.ron` in one directory, and neither reads the other's.
Nothing installs it, and a daemon that finds none runs on its own
defaults. `docs/nacelle-ai.ron.example` is a copy with every line
commented out and each one explained.

The order, for every setting there is:

```
the command line  >  the environment  >  your file
                  >  a system file    >  the built-in default
```

The environment sits above the file because `OLLAMA_HOST` and
`NACELLE_AI_FFMPEG` are what somebody exports for one run, and a line
written down months ago must not quietly beat what was typed a second
ago. The command line sits above both — and the file exists at all
because the desktop starts this daemon **with no arguments**, so until
2026-08-18 `--backend` and `--model` were flags nobody on a real machine
could reach, and the turn ceiling and the history budget were constants
reachable from nowhere at all.

| field | what it sets |
|---|---|
| `backend` | what an `ask` saying `auto` resolves to: `auto`, `claude`, `local` |
| `model` | which model to ask for |
| `ollama_host` | the local server, in `OLLAMA_HOST`'s own shapes |
| `socket` | where the socket goes, whole path — see below |
| `ffmpeg` | which ffmpeg the `loop` tool execs, absolute path |
| `limits.max_turns` | how many tool rounds one question may cost (16) |
| `limits.history_bytes` | roughly how much conversation to keep (200 000) |

A field that is not written down is answered by the next file down, so
**clearing a setting means deleting the line** rather than emptying it —
an empty line is a different sentence. Where a setting has a "none" to
say, `Off` says it, and `Off` beats a system file that names something.
That is the desktop's own `Choice` model, taken whole rather than
reinvented.

Two of those deserve a warning. Naming a `socket` moves the daemon away
from the path the widgets compute from the protocol page, so they will
not find it — the reason to set it is to run a second daemon beside the
first on purpose. And an `ollama_host` that is not on the loopback
interface costs layer 3 of the confidentiality line: the local reviewer
refuses to run anywhere but on this machine.

## Protocol v0 — JSON Lines

One JSON object per line, both directions. Commands, client → daemon:

```
{"cmd":"hello","client":"<name>","proto":0}
{"cmd":"ask","id":N,"text":"...","backend":"claude"|"local"|"auto"}
{"cmd":"tool","id":N,"tool":"loop"|"photo"|"sort","args":{...}}
{"cmd":"approve","id":N,"allow":true|false}
{"cmd":"cancel","id":N}
```

Events, daemon → client:

```
{"ev":"hello","proto":0,"backends":[...]}
{"ev":"delta","id":N,"text":"..."}       {"ev":"done","id":N,...}
{"ev":"approval","id":N,"desc":"..."}    (waits for cmd:approve)
{"ev":"progress","id":N,"msg":"..."}     {"ev":"error","id":N,"msg":"..."}
```

Every `ask` and every `tool` ends in exactly one `done` or `error`
carrying its id; a cancellation is a `done` with `"cancelled": true` in
the extras. `done` may carry more fields than `id` — a client keys on
`ev` and `id`, so the tail is free to grow. A connection runs one
command at a time; a second `ask` or `tool` sent while one runs is
answered with an error rather than queued silently. The widgets each
hold a connection of their own.

**`hello` is a negotiation, not a greeting.** The version a client names
in `proto` is checked against the ones this daemon speaks — one, `0`.
A version it does not speak is answered with `ev:error` (id `0`, which
is what every id-less answer carries) naming the client and listing the
versions there are, and until that client says hello again with one of
them, an `ask` or a `tool` on that connection is refused: a side that
told us it will misread the answer should not be handed one. A client
that says nothing at all is served exactly as before — v0 has no
required handshake and this does not add one, since a rule any client
could escape by staying silent would not be a rule. The name a client
gives itself is also the one line of the log that says **which** of the
four widgets a connection belongs to.

**Approvals ride the wire.** A change the agent wants to make and a
payload about to leave the machine both arrive as `ev:approval` and
wait for `cmd:approve` — per action, every time. There is no "allow
all": the protocol cannot say it and the daemon does not invent it. An
approval the client never answers — the widget died, the connection
closed — is a **refusal**, by construction.

## The tools

`cmd:tool` is the deterministic half of the daemon: no model is
involved on this path at all.

* **`loop`** — media in, a loop out, ffmpeg **by exec**. A video
  becomes a seamless loop: its own opening seconds are cross-faded over
  its ending (v0 drops the audio track rather than pretending an audio
  splice is seamless). One or more photos become a one-minute clip that
  cycles through them, made to be played on repeat. The result is
  always a **new** file beside the source — `<stem>-loop.<ext>`,
  counted up until a free name is found, with ffmpeg's own `-n` as the
  second lock. Nothing overwrites the input. When ffmpeg is not
  installed the answer is an error that says so and what to do;
  `NACELLE_AI_FFMPEG` points at a private build.
* **`photo`**, **`sort`** — named in the protocol, not built yet. The
  skeleton takes the command and answers `error: not built yet`, which
  is all it may honestly do.

Running ffmpeg is deliberate and stays within this project's licence
rules: executing somebody else's program is fine; copying or
translating its code is forbidden. Everything here builds argument
lists and reads exit codes.

## Two backends

The agent talks to a model through one trait, so the loop that drives it
does not know who is answering:

* **Anthropic** — the Claude API, over HTTPS.
* **Ollama** — a model running on the same machine, over localhost. No
  account, no token, nothing leaves the computer.

The two providers describe a streamed reply very differently — Anthropic
sends SSE frames, Ollama sends one JSON object per line — and they
disagree about almost everything inside them. The backend absorbs that
difference: whichever one answers, the caller sees the same sequence of
events, and a tool call is announced only once its arguments are
complete.

Ollama is reached at `OLLAMA_HOST` — the same variable its own tools
read, in the same shapes (`box:11434`, `:11434`, a full URL) — and at
`http://localhost:11434` when that is unset. Asking it what it can run
is part of the backend, because only the machine knows which models have
been pulled.

## The loop

One question is rarely one request. The model answers, asks for a tool,
is given the result and answers again, and that cycle is the agent.
Four rules shape it, and each of them is there because the obvious
version of the loop fails in a way that costs the user something:

* **It stops.** There is a hard ceiling on how many times the model may
  come back asking for another tool — sixteen unless you say otherwise.
  A model handed a tool that keeps failing the same way will keep
  calling it, and an agent without a ceiling turns that into a program
  that never returns and a bill that never stops.
* **It changes nothing on its own.** The tool registry says which calls
  only read and which would change something, and every change is put to
  the user before it happens, one call at a time. A refusal goes back to
  the model as that tool's result, in words: it is told the user
  declined, so it proposes something else instead of retrying blindly.
  There is no session-wide "allow all", and the only stock answerer in
  the crate is the one that says no to everything.
* **It keeps the conversation sendable.** When the history outgrows its
  budget the oldest of it goes — but by whole exchanges, never by single
  messages, because a tool call separated from its result is a request
  both providers reject. Whatever stops an exchange half way — a
  refusal, the stop button, the turn ceiling — every call the model made
  still ends up with a result beside it, so the next question works.
* **It does not hold up the interface.** The loop runs on a worker
  thread and reports through a `std::sync::mpsc` channel that the
  desktop's event loop drains whenever it likes. Stopping takes effect
  at the next fragment that arrives, not at the end of the reply, and
  approvals travel down that same channel: the worker waits, the
  interface keeps drawing. An approval that is dropped rather than
  answered counts as a refusal — never as a yes.

The system prompt is built from what the tool registry found on this
machine, not from a list written into the crate, and then it is left
alone for the rest of the session. Providers cache a prompt by matching
exact bytes, so a description rebuilt each turn would be paid for in
full every time and nothing would say so. Installing a theme mid-session
therefore needs a new session, which is also the honest answer: the
conversation up to that point was had with the old description.

## Which model, and how hard it thinks

The Anthropic backend offers three models and chooses none of them for
you. Unasked, it uses `claude-opus-4-8`:

| id | what it is for |
|---|---|
| `claude-opus-4-8` | the most capable — long autonomous work, hard reasoning, heavy tool use |
| `claude-sonnet-5` | close to Opus and cheaper; the one to use when a turn happens often |
| `claude-haiku-4-5` | the quick one — short, well-defined turns where waiting shows |

Those identifiers are complete as written. A date appended to one of
them is a different model, and usually not one that exists.

Thinking is off unless it is asked for, and it has to be asked for in as
many words: leaving the field out of a request is how these models are
told not to think. How deep it goes is a separate dial — `effort`, from
`low` to `max`, `high` by default and `xhigh` worth reaching for on
coding and agentic work. There is no token budget for thinking; that
parameter was removed from these models and sending it is an error.

## Credentials

Only the Anthropic backend needs one; Ollama needs nothing.

The intended way in is an OAuth token from Claude Code:

```
claude setup-token
```

That mints a long-lived token (it needs a Claude subscription) which
goes in `ANTHROPIC_AUTH_TOKEN` or in the credentials file below. An API
key works too and is the alternative, not the default.

One caveat stated plainly, because it is easier to read here than to
discover on the first request: the token `claude setup-token` issues is
minted for Claude Code. The header shape this agent sends is the
documented one for OAuth tokens, but whether Anthropic accepts that
token for ordinary Messages API calls from a third-party program is not
something this repository can promise. If it is refused, the failure is
reported as an authentication error that says so — and Ollama keeps
working regardless, which is why the local backend is not optional.

The agent looks in three places, in order, and takes the first it finds:

1. `ANTHROPIC_API_KEY` — an API key.
2. `ANTHROPIC_AUTH_TOKEN` — an OAuth token (what `claude setup-token`
   gives you).
3. `$XDG_CONFIG_HOME/nacelle-ai/credentials.json` (or
   `~/.config/nacelle-ai/credentials.json`).

A blank variable counts as unset, so a leftover `export
ANTHROPIC_API_KEY=` in a shell profile does not shadow a working token.

The two kinds are not interchangeable — an API key is sent as
`x-api-key`, an OAuth token as `Authorization: Bearer` together with the
`anthropic-beta: oauth-2025-04-20` flag — so the agent tracks which kind
it holds and builds the headers itself.

The file holds one of:

```json
{ "api_key": "sk-ant-..." }
```

```json
{ "oauth_token": "sk-ant-oat01-..." }
```

**It must be readable by you and nobody else.** If any group or world
permission bit is set, the agent refuses to use the file and says so,
rather than spending a token that the rest of the machine can also read:

```sh
mkdir -p ~/.config/nacelle-ai
install -m 600 /dev/null ~/.config/nacelle-ai/credentials.json
$EDITOR ~/.config/nacelle-ai/credentials.json
```

The secret never reaches a log, an error message, a panic or this
repository. It is held in a type whose `Debug` and `Display` print
`<redacted>`, and the paths that could carry it out are the outgoing
request headers and nothing else.

## What it can do to the desktop

The agent manages a nacelle installation through tools, and today it
does that by editing the files nacelle-desktop reads. There is no
channel to a *running* desktop yet — that is a later stage — so a change
applies the next time the desktop starts. Every tool that writes says
exactly that in its own description, because the one thing the agent
must never do is tell you the screen has already changed.

| tool | what it does |
|---|---|
| `nacelle_list_themes` | the installed `.theme` files, and which one is selected |
| `nacelle_set_theme` | sets the theme in `nacelle-desktop.ron` |
| `nacelle_list_layauts` | the installed layauts, and which one is selected |
| `nacelle_read_layaut` | one layaut file, as text |
| `nacelle_set_layaut` | sets the layaut, and only to a layaut that is installed |
| `nacelle_list_addons` | the installed scripts and plugins, and what each declares about itself |
| `nacelle_read_config` | every setting in force, the file it came from, and the keys that may be set |
| `nacelle_set_config` | sets one key in `nacelle-desktop.ron` |

The configuration the tools edit is **RON** —
`nacelle/nacelle-desktop.ron`, per the owner's decision — read and
written through the same typed model the desktop derives its parser
from, so the two programs agree byte for byte about what the file
means. A directory that still has only the old `Key=Value`
`nacelle-desktop.conf` is read through it, and never written: a
machine that had settings before the change keeps exactly the file it
had, and the first write puts a `.ron` beside it.

A listing covers installed FILES. The toolkit carries themes compiled
into it and the desktop links some widgets straight into its binary;
neither is a file, and the tools say so rather than claiming a
completeness they do not have. There is no tool for reading a log,
because the desktop keeps none — it prints its diagnostics to standard
error, where they belong to whatever started it.

Writing is fenced in four ways, in this order:

1. **Confinement** — the path is resolved canonically, `..` collapsed
   and symlinks followed, and must land inside the user's configuration
   directory. Anything else is refused rather than written.
2. **Validation** — the value is checked and the whole new text is read
   back before anything on disk is touched, so a rejected value never
   reaches the filesystem. Only the keys the desktop actually reads may
   be set, and only to values of the shape it honours.
3. **Backup** — the previous contents are copied to `<name>.bak`.
4. **Atomic replace** — the new text is written beside the target and
   renamed over it, so an interrupted write leaves the old file or the
   new one and never half of either.

No tool deletes anything, no tool writes outside the configuration
directory, and no tool runs an addon in order to describe it.

## The supervisor: what leaves the machine, and when

The local agent (Ollama) is the one that reads anything at all, and
Claude sees only what is sent to it. What "reads" means today is
narrower than the design page once claimed: the tools below are the
whole of it — the themes, the layauts, the addons and the configuration
file — and there is no general file-reading tool in this program. The
design is written out in [`docs/supervisor.md`](docs/supervisor.md),
which marks what is built and what is not; what follows is what the code
does.

The obvious way to keep secrets out of a payload — show the text to a
model and ask — is the weakest one available, and this project does not
use it. A model's judgement is a probability, not a boundary, and the
cases it gets wrong are the cases that matter: a key pasted into a log, a
token in a URL, a password in a shell history line. One miss is on
somebody else's server and cannot be recalled. **So the model is the
last layer, not the first.**

1. **Never read it.** A denylist is checked before every read, on the
   canonical path, so a symlink is the file it points at. Credential
   stores (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gh`, and this
   program's own credentials file), password managers, browser profiles,
   `.env` and `.env.*`, key material by extension and by PEM armour, and
   shell histories. It has no "off": there is no way to remove an entry,
   and a tool argument or a prompt that asks nicely gets the same
   refusal. The refusal is **reported**, never swallowed — you may be
   asking for a perfectly good reason, and then you open the file
   yourself.
2. **Never send it.** Everything about to cross the network is scanned
   deterministically: PEM armour, provider-shaped credentials
   (`sk-ant-`, `sk-`, `ghp_`, `gho_`, `glpat-`, `AKIA`, `xox[baprs]-`,
   `xapp-`, `AIza`), `Authorization:` headers and bearer tokens, JWTs,
   passwords in connection strings, values written under a name that
   says they are secret, and long high-entropy runs with no
   natural-language profile. A hit is **cut**, and the marker left
   behind says what went and why — so the far model knows to ask you for
   it rather than answering as though nothing was missing. The marker
   never quotes what it replaced: every word in it is one of this
   program's own constants.

   It also reads the payload **with its wrapping taken out**, because a
   key that a terminal, an editor or a mail client broke into pieces is
   not one run of characters and every rule above would otherwise look
   at each piece and pass. Line breaks, spaces and invisible characters
   woven through a key are all removed before the provider shapes are
   looked for, and a key that runs off the end of one message block into
   the next is followed into it.
3. **Then ask the local model.** Only now, and only about meaning:
   somebody's health, a private message, an unreleased plan. It is given
   the already-redacted text and can answer with one thing — a list of
   quotes to remove. There is no shape of answer that puts anything back.
4. **Then show you the manifest.** Before the first escalation of a
   session, and whenever the payload carries a file you have not already
   answered for: which files, how many bytes, what was removed. It goes
   on your word. The size on it covers every string that reaches the
   socket, including the two the layers may read but not edit — the
   model id and a thinking block's signature.

**Pressing stop actually stops the bytes.** Layer 3 is a whole turn
against the local model and layer 4 waits on you, which together are
most of the time between asking a question and anything leaving the
machine. Stop is read between the layers and while the manifest is on
screen, so a turn you stopped in that window is one that did not go —
not one that is merely no longer shown to you.

Those four are an order, and the order is a type rather than a comment.
A payload addressed to the remote model only exists as a `Cleared`, and
the only way to make one is to walk the whole line — a decision the
policy allowed, a payload that went through layers 1 to 3, and a
manifest that was *handed to* the interface rather than fetched by it.
There is no argument that skips a step, and there is no stock "always
send": with nobody at the machine to answer, the answer is no.

Inside that line the order is a type as well. A payload is a `Gathering`
while text can still go into it and an `Outgoing` once layer 3 has
finished with it, and the transition runs one way only — so there is no
state in which a payload is both *the local model has seen this* and
*there is text in here nothing has seen*. The code that would produce
one does not compile. What the manifest then says is read off the
finished payload rather than tallied up beside it: the size is the
length of the string that goes, and a file's line is what that file's
bytes weigh where they actually sit, after every layer took what it
wanted. A manifest is the only part of this you can see, so it is the
one part that must not be approximately right.

And the line is where the bytes are, which is the part that is easy to
get wrong: the Anthropic backend holds a `Seal` it cannot be built
without, and the function that encodes its HTTP body takes a `Sealed`
request — produced only by running layers 2, 3 and 4 over every string
in the request. A second code path in that backend cannot skip them,
because there is nothing for it to encode until they have run. The local
backend has no seal at all, deliberately: the layers exist for bytes
reaching a third party under a credential, and a model on your own
machine is neither.

### What this does not protect

Written down because a boundary you can see the edges of is worth more
than one you have to trust. Every line here has been measured against
the bytes handed to the socket, not reasoned about.

**Read this part first, because it governs the rest of the list.** This
layer is a best effort, not a guarantee, and the difference matters to
what you paste into the conversation. Three rounds of adversaries have
now attacked it. Each round found holes the previous round had not — and
each round found them in the *marker*, the *ordering*, the *shape of the
payload*, not in some corner nobody thought about. The third round is the
one that discovered the redaction marker was itself quoting the key it
had just removed, on the wire, for anything with a long enough field
name. That was fixed the only way such a thing can be: the marker's type
no longer has anywhere to put a byte of the payload. But a fourth round
would find a fourth thing, and pretending otherwise would be the one
failure this layer cannot recover from — because somebody who believes it
is airtight will paste what they would otherwise have withheld.

So: the local model answers by default and the remote one is asked only
when you ask for it; what leaves is listed for you before it goes; and
what leaves has been through the layers below. Treat that as three
reductions of risk, not as a seal. If something must not reach Anthropic
under any circumstances, do not put it in front of the agent at all —
`--backend local` pins a session to the machine and is the only thing
here that is a guarantee rather than an effort.

Two bypasses are open right now and measured this way today:

* **Base32 goes.** The entropy rule refuses to judge a run with fewer
  than three of its four character classes, and base32 has two — capital
  letters and digits. A thirty-two character base32 secret with no name
  and no known prefix passes. Hex does not: it is caught by the rule
  that catches digests, which is why a git revision is cut too.
* **A key wearing a sentence goes.** The entropy rule stands down for
  anything that looks like prose, and hyphens keep a run in one piece —
  so `attachment-the-quick-brown-fox-jumps-over-the-lazy-dog-<key>` reads
  as prose to it and carries the key through. Neither of these has a fix
  that does not also start cutting ordinary text, which is why they are
  written here rather than patched in a hurry.

* **A short secret with no name and no prefix goes.** `Qv2Lm4Np7R` in
  the middle of a sentence is not cut, and cannot be: at ten characters
  it is the same string as `Ubuntu2404`, `GTK4Widget` or `SHA256Sum1`,
  and a rule that removed one would put a marker in the middle of most
  sentences anybody types about software. Give it a name —
  `password: Qv2Lm4Np7R` — and it goes. The length at which a nameless
  value starts being cut is 32 characters.
* **A digest is cut, and that is the price of catching hex tokens.** A
  forty-character git revision and a forty-character GitHub token are
  the same forty characters. This layer cuts both. An abbreviated
  revision — the seven characters a person actually types — is left
  alone.
* **A credential split so that no piece is recognisable goes.** The
  block-continuation rule follows a key from one message block into the
  next, but only when the first block held enough of it for a rule to
  see. A key broken up so that its first piece is under the length its
  own provider prefix demands is not caught by anything.
* **The manifest asks once a session about prose.** It asks again when a
  file you have not answered for turns up, and a file is a tool result
  whose call named a path. Something private that never came from a
  named file — you typed it, or the model wrote it — is disclosed on
  the first escalation of the session and not again.
* **The manifest cannot name what the model quoted.** It lists the files
  a tool was asked for by path. Anything summarised, quoted or pasted
  into the conversation is in what is being sent and is not on the list,
  and the manifest says so on its own face.
* **Nothing here scans the local model's own reading.** Layers 2 to 4
  exist because bytes are about to leave the machine. A turn answered by
  Ollama on your own host is not scanned at all, deliberately.
* **HTTP headers and the URL are not scanned.** The credential goes in a
  header on purpose. Only the request body passes through the layers.

Escalation itself does not hang on the local model saying "I can't do
this" — small models are unreliable narrators of their own competence in
both directions. The triggers are mostly things a counter can see: you
asked, the same task failed twice, the work does not fit the context
window, the loaded model lacks a capability, or the model asked *and
stated a reason you can read*.

A session can be pinned local, and then the agent says what it cannot do
instead of reaching for the network. **A machine with no token and a
machine with no network degrade to exactly that** — same code path,
different sentence — because the local half must never depend on the
remote half being reachable.

**There is no background watch running.** The mechanism exists in the
core — event-driven, pausable, stoppable, and calling no model of its
own — and nothing outside its tests constructs one. The daemon leaves it
unplugged deliberately: the owner's first rule is that nothing happens
without a command, and a watcher is a thing that happens without one.
What it would have to look like if it is ever plugged in is in
[`docs/supervisor.md`](docs/supervisor.md), marked as design.

## Who answers an ask

`auto` is the daemon's own agent: the local model with the nacelle
configuration tools, whether or not a credential resolves. Claude is
not the first responder and does not run unasked — a token being
present does not make it one.

`claude` is the client asking, in as many words, and it is honoured
from the first command — behind the manifest, which arrives on the
connection as `ev:approval` and goes nowhere until `cmd:approve` says
so. `local` is chat pinned to this machine, explicitly, per command.

Naming a provider uses that one or says why it cannot — asking for
Claude and silently getting a model on your own machine would answer a
different question than the one asked. A daemon started with
`--backend local` refuses claude asks with the pin, not with a missing
token.

## Build

```sh
cargo build
cargo test

make check          # the gate: a build from nothing, then the whole suite
make install        # a clean build, installed; see "Installing it" above
```

`make check` is `rm -rf target` and then both, which is the project's
rule: a gate that ran against a stale `target` proved something about a
tree nobody has.

The interface that draws the daemon's answers lives in
[nacelle-addons](https://github.com/JOCKER3201/nacelle-addons): four
widgets on the desktop's upper board, each holding a connection of its
own. This repository builds one binary, `nacelle-ai`, and it opens no
window.

## Licence

MIT — see [LICENSE](LICENSE). Every dependency is MIT, Apache-2.0, BSD,
ISC or Zlib; nothing copyleft may be added, and nothing that brings an
async runtime with it.

TLS is the one place that needed deliberate work. The usual rustls setup
compiles in a copy of Mozilla's CA list, which ships under a licence
outside that set, so it is switched off and the trust store is read from
the machine instead. That is also the better behaviour for something
that ships as part of a desktop: a root the user installed works, and one
the distribution withdrew stops working without waiting for a release
here.
