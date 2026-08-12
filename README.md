# nacelle-ai

The AI agent for the [nacelle](https://github.com/JOCKER3201/nacelle-desktop)
environment — an agent that can read the machine it runs on, use tools,
and answer in the desktop's own vocabulary.

> **THIS PROJECT IS WRITTEN WITH THE HELP OF AI (Anthropic's Claude
> models). Every line of code here was produced in collaboration with an
> AI assistant. This notice is deliberate and permanent — the project's
> owner requires that the authorship of this code is never left
> ambiguous.**

## Status: early

Very early. `nacelle-ai` opens a window, picks a backend, and holds a
streamed conversation with tools and an approval step. The widget half
does not exist yet. Nothing here is stable — the crate layout, the
contracts and the configuration format may all still change.

## Two modes

The same agent, in two shapes, from one core:

* **Standalone** — its own process and its own window, built on
  [libnacelle](https://github.com/JOCKER3201/libnacelle). See below.
* **Widget** — loaded by nacelle-desktop and drawn as a panel among the
  others. Not written yet.

Everything that is not about being a process lives in `nacelle-ai-core`,
so neither mode has an implementation the other lacks. The core draws
nothing and knows nothing about windows.

It also has no async runtime and never will: the desktop owns a
synchronous event loop, so the agent runs on a worker thread and hands
results back over a `std::sync::mpsc` channel. A dependency that drags
in a reactor cannot be used here.

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
| `nacelle_set_theme` | sets `Theme=` |
| `nacelle_list_layauts` | the installed layauts, and which one is selected |
| `nacelle_read_layaut` | one layaut file, as text |
| `nacelle_set_layaut` | sets `Layaut=`, and only to a layaut that is installed |
| `nacelle_list_addons` | the installed scripts and plugins, and what each declares about itself |
| `nacelle_read_config` | every setting in force, the file it came from, and the keys that may be set |
| `nacelle_set_config` | sets one key in `nacelle-desktop.conf` |

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

The local agent (Ollama) is the one with eyes. It runs beside the
desktop, it can read what you can read, and Claude sees only what it
decides to send. The design is written out in
[`docs/supervisor.md`](docs/supervisor.md); what follows is what the code
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

The background watch is event-driven and not a model in a loop. Cheap
deterministic checks run continuously — a threshold in telemetry the
desktop already collects, a widget reporting its own anomaly — and
nothing is interpreted until one of them fires. It can be paused and
stopped from the interface, and it says which it is: a background process
that reads your files and can be neither seen nor stopped is not a
feature.

## The window

```
nacelle-ai [--backend auto|claude|local] [--model <id>]
```

`auto` is the local model, whether or not a credential resolves. Claude
is not the first responder and does not run unasked: it is reached when
something makes it necessary — you asking, the same task failing twice,
work that does not fit the local context, a capability the local model
has not got, or the local model asking with a reason you can read — and
you see exactly what would leave the machine before it does.

`claude` is you asking, in as many words, and it is honoured from the
first turn. `local` pins the session: nothing goes off the machine, and
the agent says what it cannot do instead of reaching for the network. A
machine with no token and a machine with no network degrade to exactly
that pin, because the local half must never depend on the remote half
being reachable.

Naming a provider uses that one or says why it cannot — asking for
Claude and silently getting a model on your own machine would answer a
different question than the one asked.

Enter sends. Escape stops an answer that is arriving, and answers a
waiting change: while the agent is holding a change for approval, Enter
and Escape belong to that question and every other key still edits the
field, because a person may well type their next question while they
think about the one on screen.

The reply grows as it streams. The agent loop blocks on a worker thread
and reports over a channel; a relay thread hands each event to winit's
own queue, so the window sleeps until something happens instead of
polling for it, and every fragment of the reply is a wake-up.

Five states are **shown** rather than left to be inferred, each saying
what to do about it: no local model, no Anthropic credential, a refusal
from the model, a cancellation, and the turn ceiling. Which provider is
answering is on screen permanently, because it is a fact about the
conversation and not about one turn.

This is the first program outside nacelle-desktop built on the toolkit,
and that is the point of it: the window is winit and a Vulkan surface
through [nacelle-renderer](https://github.com/JOCKER3201/nacelle-renderer),
and everything above that — the theme engine and its master
`default.theme`, the scrolling row list the conversation is drawn as, the
text input with its caret, selection and undo — comes from libnacelle
with nothing copied out of the desktop. **Every colour, length and
duration on screen comes from a theme token, including as a fallback:
what the theme does not say, this program does not draw.**

Three numbers in `window.rs` are not tokens, and each says why in a
comment. The size the window first asks the window manager for, because
the theme describes what is inside a window and has nothing to say about
how large one should open — everything drawn within it is derived from
the window's actual size, so it decides nothing but the first frame. The
pace of a frame while something is still moving, which is a clock rather
than a look. And the distance under which the view still counts as being
at the bottom, which is arithmetic: a difference below one device pixel
cannot be seen, and if it counted as "the reader scrolled away" a reply
would stop following its own tail over a rounding error. All three are
tokens the master could grow — see the handover notes.

## Build

```sh
cargo build
cargo test
```

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
