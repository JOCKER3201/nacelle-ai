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

Very early. The agent runs in a terminal, reports which credential it
found, and stops; there is no window. Both backends are written and hold
a real conversation, tools and all, but nothing in the binary drives one
yet. Nothing here is stable — the crate layout, the contracts and the
configuration format may all still change.

## Two modes

The same agent, in two shapes, from one core:

* **Standalone** — its own process and, later, its own window built on
  [libnacelle](https://github.com/JOCKER3201/libnacelle).
* **Widget** — loaded by nacelle-desktop and drawn as a panel among the
  others.

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
