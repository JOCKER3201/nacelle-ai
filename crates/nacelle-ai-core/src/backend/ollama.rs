//! The Ollama backend: a model running on this machine, reached over
//! plain HTTP.
//!
//! This is the half of the project that needs no account and no token.
//! Nothing in this module reads a credential, and on the default host
//! nothing it sends leaves the computer. An `OLLAMA_HOST` pointing
//! somewhere else is honoured, https included, because a user who runs
//! their models on the machine in the next room is still running their
//! own models.
//!
//! **Nothing here redacts, and that is a decision rather than an
//! omission.** The Anthropic backend cannot encode a request that has
//! not been through layers 2, 3 and 4 — see
//! [`supervise::seal`](crate::supervise::seal) — and this one has no
//! seal at all. The layers exist because a payload is about to reach a
//! third party's endpoint, under a credential, where one miss cannot be
//! recalled. None of that is true of a model on the user's own machine:
//! redacting there would hide the user's own files from the only agent
//! they asked to look at them, and buy nothing, since the bytes are
//! already where they started.
//!
//! The one case worth stating out loud is an `OLLAMA_HOST` pointing at
//! another machine. Those bytes do cross a wire, and they cross it
//! unredacted — because the user named that host themselves, no
//! credential is involved and no third party is on the other end. What
//! that costs is honesty about the word "local", which is why
//! [`Backend::is_local`] answers strictly: layer 3 asks a different
//! question — *may this model be shown the payload* — and for the
//! machine in the next room the answer to that one is no.
//!
//! Two things about the wire shape the code.
//!
//! **The stream is NDJSON, not SSE.** One JSON object per line, each
//! carrying the increment since the last one, and a final object with
//! `done: true` and the counters. There are no `event:` prefixes and no
//! blank-line frame separators — a line *is* an object. What that costs
//! is the one thing every streaming parser has to get right anyway: a
//! read returns whatever bytes have arrived, which is regularly half a
//! line, so lines are reassembled rather than assumed.
//!
//! **Tool calls arrive whole.** Ollama puts the parsed arguments in
//! `message.tool_calls[].function.arguments` in a single object, so
//! unlike Anthropic there is nothing here to buffer and nothing that
//! could be announced early. Identifiers are less settled: recent
//! servers send one per call, older ones send none, and the contract
//! needs one either way to match a result back to its call — so the
//! server's is kept when it is there and one is minted when it is not.
//!
//! Everything that can be decided without a socket is a free function —
//! [`request_body`], [`translate_stream`], [`parse_models`],
//! [`http_error`] — and [`Ollama`] is the thin piece between them that
//! opens one. That split is deliberate: it is what lets the whole
//! translation, in both directions, be tested against recorded bytes
//! instead of a running server.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::backend::{Backend, EventSink, Flow};
use crate::credentials::{Env, ProcessEnv};
use crate::error::BackendError;
use crate::event::{StopReason, StreamEvent, Usage};
use crate::message::{Content, Message, Request, Role, ToolCall, ToolDeclaration};

/// Where Ollama listens when nobody has said otherwise.
pub const DEFAULT_HOST: &str = "http://localhost:11434";

/// The variable Ollama's own command-line tool reads. Honouring it means
/// a user who has already pointed `ollama` at a machine does not have to
/// say it a second time for the agent.
pub const HOST_VAR: &str = "OLLAMA_HOST";

/// The port Ollama listens on when the host says no port.
const DEFAULT_PORT: &str = "11434";

/// What [`Backend::name`] answers. Never the host: a name identifies the
/// provider, not where this particular machine happens to reach it.
const NAME: &str = "ollama";

/// A model this server has, as much of it as a picker needs.
///
/// The extra fields are what tells two entries apart when a user has
/// pulled the same model at three quantisations: the name alone is
/// `gemma4:31b-it-q4_K_M`, which is precise but says nothing about how
/// much memory it will want.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The tag to put in [`Request::model`], verbatim.
    pub name: String,
    /// On-disk size in bytes. Zero when the server did not say.
    pub size: u64,
    pub family: Option<String>,
    /// The maker's parameter count as text — `"30.5B"` — because that is
    /// how the server reports it and rounding it to a number would only
    /// lose what the maker meant.
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
}

impl ModelInfo {
    /// One entry of `/api/tags`. Entries without a name are not models
    /// this backend can ask for, so they are dropped rather than guessed
    /// at.
    fn from_json(value: &Value) -> Option<Self> {
        let name = value
            .get("name")
            .or_else(|| value.get("model"))
            .and_then(Value::as_str)?
            .to_string();
        let details = value.get("details");
        let detail = |key: &str| -> Option<String> {
            details
                .and_then(|details| details.get(key))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };

        Some(ModelInfo {
            name,
            size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
            family: detail("family"),
            parameter_size: detail("parameter_size"),
            quantization: detail("quantization_level"),
        })
    }
}

/// A local Ollama server.
///
/// Cheap to build and holds no conversation: the history lives with the
/// caller, exactly as [`Request`] describes. One instance per worker
/// thread — the connection pool inside it is not the thing that makes a
/// backend single-turn, the contract is.
pub struct Ollama {
    /// Already normalised: scheme, host, port, optional path prefix, and
    /// never a password.
    host: String,
    agent: ureq::Agent,
}

impl Ollama {
    /// The server named by `OLLAMA_HOST`, or the local default.
    pub fn new() -> Self {
        Self::from_env(&ProcessEnv)
    }

    /// The same, from an environment given explicitly.
    ///
    /// The seam exists so tests can say what the environment is instead
    /// of mutating the process's own — which is shared, racy under a
    /// threaded test runner, and would let a developer's real settings
    /// decide whether a test passes.
    pub fn from_env(env: &dyn Env) -> Self {
        Self::at(env.var(HOST_VAR).unwrap_or_default())
    }

    /// A server at an explicit address, in any of the shapes Ollama's own
    /// tools accept: `box:11434`, `:11434`, `http://box`, or a full URL
    /// with a path prefix for a reverse proxy.
    pub fn at(host: impl AsRef<str>) -> Self {
        Ollama {
            host: normalise_host(host.as_ref()),
            agent: build_agent(),
        }
    }

    /// The address requests actually go to, after normalisation. Safe to
    /// show: [`normalise_host`] has already dropped anything that looked
    /// like a password.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Every model this server has, sorted by name.
    ///
    /// Which model to use is a choice for whoever is driving — a picker
    /// in the widget, a flag on the binary — and this is the only way to
    /// offer that choice honestly, because the answer depends on what
    /// the user has pulled and nothing else can know it.
    pub fn models(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let mut response = self
            .agent
            .get(format!("{}/api/tags", self.host))
            .call()
            .map_err(|err| self.transport_error(err))?;

        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|err| self.transport_error(err))?;

        if !is_success(status) {
            return Err(http_error(status, retry_after(response.headers()), &body));
        }

        parse_models(&body)
    }

    /// Everything that went wrong before a status code existed.
    ///
    /// The message never carries the error's own text without context:
    /// "connection refused" on its own has been the least helpful line
    /// in every log it has ever appeared in, and the useful half — which
    /// machine, and that `ollama serve` is how it starts — is exactly
    /// what this type knows and the io error does not.
    fn transport_error(&self, err: ureq::Error) -> BackendError {
        match err {
            ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
                BackendError::Network(format!(
                    "nothing is listening at {} — is the Ollama server running? (`ollama serve`)",
                    self.host
                ))
            }
            ureq::Error::Timeout(_) => BackendError::Network(format!(
                "{} accepted the connection but did not answer in time",
                self.host
            )),
            other => BackendError::Network(format!("could not reach {}: {other}", self.host)),
        }
    }
}

impl Default for Ollama {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for Ollama {
    fn name(&self) -> &str {
        NAME
    }

    /// True only when the server is on the loopback interface.
    ///
    /// A stricter answer than "is this the user's own model", and
    /// deliberately so. The callers of this ask one question — do these
    /// bytes cross a wire — and for an `OLLAMA_HOST` pointing at the
    /// machine in the next room the honest answer is yes, however much
    /// that machine belongs to the same person. Being wrong in this
    /// direction costs a refused shortcut; being wrong in the other
    /// costs a payload on a network the user did not think they were
    /// using.
    fn is_local(&self) -> bool {
        is_loopback(&self.host)
    }

    fn send(&mut self, request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        let body = request_body(request).to_string();
        let mut response = self
            .agent
            .post(format!("{}/api/chat", self.host))
            .header("content-type", "application/json")
            .send(body.as_str())
            .map_err(|err| self.transport_error(err))?;

        let status = response.status().as_u16();
        if !is_success(status) {
            // Ollama says what is wrong in the body — which model is
            // missing, which capability it lacks — and a status code on
            // its own would throw that away. It is a short body: reading
            // it in full costs nothing and is the whole message.
            let retry_after = retry_after(response.headers());
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(http_error(status, retry_after, &text));
        }

        translate_stream(response.body_mut().as_reader(), &request.model, sink)
    }
}

/// The agent every request goes through.
fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // A non-2xx reply must come back as a reply, not as an error.
        // The default behaviour raises the status and drops the body,
        // and the body is where Ollama explains itself.
        .http_status_as_error(false)
        // Connecting to a port on this machine either succeeds at once
        // or is refused at once. A long connect timeout only delays the
        // news that the server is not running.
        .timeout_connect(Some(Duration::from_secs(5)))
        // No deadline on the reply itself. A 30B model on a busy machine
        // can take minutes to produce its first token, and a timeout
        // here would report a working setup as a broken one.
        .timeout_global(None)
        .timeout_recv_body(None)
        .build()
        .new_agent()
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// `Retry-After` as a duration, when the header is there and is a plain
/// number of seconds. The HTTP-date form is not read: Ollama never sends
/// either, this is here for a proxy in front of it, and a wrong wait is
/// worse than no wait.
fn retry_after(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// The body of `/api/tags`, parsed.
pub fn parse_models(body: &str) -> Result<Vec<ModelInfo>, BackendError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| BackendError::Protocol(format!("the model list is not JSON: {err}")))?;

    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            BackendError::Protocol("the model list has no `models` array".to_string())
        })?;

    let mut out: Vec<ModelInfo> = models.iter().filter_map(ModelInfo::from_json).collect();
    // The server's order is neither alphabetical nor stable between
    // calls, and a picker whose entries move under the cursor is worse
    // than one that is merely unsorted.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// What an unsuccessful HTTP reply means, in the caller's terms.
///
/// The provider's own sentence is kept as the message, because Ollama
/// writes good ones, and a hint is appended only for the two failures a
/// user cannot otherwise act on: a model that is not installed, and a
/// model that cannot do what was asked of it.
pub fn http_error(status: u16, retry_after: Option<Duration>, body: &str) -> BackendError {
    let message = explain(status, provider_message(body));

    match status {
        401 | 403 => BackendError::Auth(message),
        429 => BackendError::RateLimited {
            retry_after,
            message,
        },
        // Everything else keeps its status. A 4xx here is not "the
        // provider failed" in spirit — it is a request this server will
        // not accept — but it is not retryable either, and
        // `is_retryable` already draws that line at 500, so the shape
        // that carries the number is the honest one.
        _ => BackendError::Server { status, message },
    }
}

/// Ollama's `{"error": "..."}`, or the raw body when it is not that
/// shape, or a stand-in when there is no body at all.
fn provider_message(body: &str) -> String {
    let text = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());

    if text.is_empty() {
        "the server gave no reason".to_string()
    } else {
        text
    }
}

/// The two capability failures, said outright.
///
/// Both arrive as a flat sentence about a model name — the user is left
/// to work out that the request has to change, not the server. Saying so
/// is the difference between an error and an instruction.
fn explain(status: u16, message: String) -> String {
    if message.contains("does not support tools") {
        format!(
            "{message} — this model cannot use tools at all; ask it without tools, or pick a \
             model that supports them"
        )
    } else if message.contains("does not support thinking") {
        format!(
            "{message} — this model cannot show its reasoning; send the request with thinking \
             turned off"
        )
    } else if status == 404 {
        format!("{message} — the model is not installed on this machine; pull it first, or pick one the server already has")
    } else {
        message
    }
}

/// The JSON body this backend posts to `/api/chat` for `request`.
///
/// Public because it is half of the translation and the half no recorded
/// reply can exercise: what the model is told is as much a part of the
/// contract as what it says back.
pub fn request_body(request: &Request) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("messages".to_string(), Value::Array(wire_messages(request)));
    // Always streamed. A turn that arrived in one piece would still have
    // to be turned into the same events, and streaming is what lets the
    // desktop show an answer while it is being written.
    body.insert("stream".to_string(), json!(true));

    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(wire_tools(&request.tools)),
        );
    }

    // Only sent when asked for. Models that cannot think reject the
    // field rather than ignoring it, so sending `false` everywhere would
    // break plain requests to plain models for no gain.
    if request.thinking {
        body.insert("think".to_string(), json!(true));
    }

    body.insert(
        "options".to_string(),
        json!({ "num_predict": request.max_tokens }),
    );

    Value::Object(body)
}

/// The conversation in Ollama's shape.
fn wire_messages(request: &Request) -> Vec<Value> {
    let mut out = Vec::new();

    // Ollama carries the system prompt as an ordinary first message,
    // which is why [`Role::System`] exists in the core model at all.
    if let Some(system) = &request.system {
        out.push(json!({ "role": "system", "content": system }));
    }

    let names = tool_names(&request.messages);
    for message in &request.messages {
        push_message(message, &names, &mut out);
    }
    out
}

/// Which tool each call id belongs to, read back out of the history.
///
/// Ollama matches a result to its call by tool name, and the core model
/// matches by id, so the two have to be reconciled somewhere. Doing it
/// from the history rather than by decoding the id keeps the id opaque,
/// which is what the contract promises the caller.
fn tool_names(messages: &[Message]) -> HashMap<&str, &str> {
    let mut names = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let Content::ToolUse(call) = block {
                names.insert(call.id.as_str(), call.name.as_str());
            }
        }
    }
    names
}

/// One core message as one or more Ollama messages.
///
/// Tool results cannot travel inside the message that holds them: Ollama
/// wants a message of its own, with role `tool`, per result. So a turn
/// that answers two tools becomes two `tool` messages, and whatever text
/// came with them follows as the caller's own message — which is the
/// order the conversation happened in anyway.
fn push_message(message: &Message, names: &HashMap<&str, &str>, out: &mut Vec<Value>) {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = Vec::new();

    for block in &message.content {
        match block {
            Content::Text(fragment) => text.push_str(fragment),
            Content::Thinking { text: fragment, .. } => thinking.push_str(fragment),
            Content::ToolUse(call) => calls.push(json!({
                "function": {
                    "name": call.name,
                    "arguments": call.input,
                }
            })),
            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let mut result = Map::new();
                result.insert("role".to_string(), json!("tool"));
                result.insert("content".to_string(), json!(tool_output(output, *is_error)));
                if let Some(name) = names.get(id.as_str()) {
                    result.insert("tool_name".to_string(), json!(name));
                }
                out.push(Value::Object(result));
            }
        }
    }

    if text.is_empty() && thinking.is_empty() && calls.is_empty() {
        return;
    }

    let mut wire = Map::new();
    wire.insert("role".to_string(), json!(role_name(message.role)));
    wire.insert("content".to_string(), json!(text));
    if !thinking.is_empty() {
        wire.insert("thinking".to_string(), json!(thinking));
    }
    if !calls.is_empty() {
        wire.insert("tool_calls".to_string(), Value::Array(calls));
    }
    out.push(Value::Object(wire));
}

/// A tool message has no room for "this failed", and a model that cannot
/// tell a failure from a result will build on the failure. Saying so in
/// the text is the only channel there is.
fn tool_output(output: &str, is_error: bool) -> String {
    if is_error {
        format!("error: {output}")
    } else {
        output.to_string()
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Tool declarations in the OpenAI-shaped envelope Ollama borrowed.
fn wire_tools(tools: &[ToolDeclaration]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    // Ollama calls the schema `parameters`; the core
                    // calls it what JSON Schema calls it. Same document.
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
}

/// Turn one NDJSON response body into contract events.
///
/// `fallback_model` is used for [`StreamEvent::Start`] only if the server
/// does not name the model itself, which it normally does — and what it
/// names is not always what was asked for, since a tag can resolve to
/// something else.
///
/// Public so that the whole of the reading half can be tested against
/// recorded bytes: every failure this has to survive — a line split
/// across two reads, a blank line, a stream that stops in the middle —
/// is a property of the byte stream, not of the network.
pub fn translate_stream<R: Read>(
    body: R,
    fallback_model: &str,
    sink: &mut EventSink<'_>,
) -> Result<(), BackendError> {
    let mut reader = BufReader::new(body);
    let mut line = String::new();
    let mut started = false;
    let mut ended = false;
    let mut tool_calls = 0usize;

    while !ended {
        line.clear();
        // `read_line` is what makes a split line a non-event: it keeps
        // reading until it has a newline, however many reads that takes.
        let read = reader
            .read_line(&mut line)
            .map_err(|err| BackendError::Network(format!("the reply broke off: {err}")))?;
        if read == 0 {
            break;
        }

        let complete = line.ends_with('\n');
        let text = line.trim();
        // Blank lines are framing, not content: a trailing newline at
        // the end of the body is one, and so is a keep-alive.
        if text.is_empty() {
            continue;
        }

        let object: Value = serde_json::from_str(text).map_err(|err| {
            if complete {
                BackendError::Protocol(format!("a line of the reply is not JSON: {err}"))
            } else {
                // The body ended part-way through an object. That is a
                // dropped connection, not a server that talks nonsense,
                // and the two call for different reactions.
                BackendError::Network(
                    "the reply ended in the middle of a line — the connection dropped".to_string(),
                )
            }
        })?;

        // Ollama can fail after the headers are out, and does when a
        // model has to be unloaded mid-turn. No `End` follows: the turn
        // did not end, it broke.
        if let Some(message) = object.get("error").and_then(Value::as_str) {
            return Err(BackendError::Server {
                status: 200,
                message: explain(200, message.to_string()),
            });
        }

        if !started {
            let model = object
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(fallback_model);
            emit(
                sink,
                StreamEvent::Start {
                    model: model.to_string(),
                },
            )?;
            started = true;
        }

        let message = object.get("message");
        // Forwarded whether or not the request asked for thinking: some
        // models reason by default and say so here, and a backend that
        // dropped what the model actually said would be deciding for the
        // receiver what it is allowed to know about the turn.
        if let Some(fragment) = field(message, "thinking") {
            emit(sink, StreamEvent::Thinking(fragment.to_string()))?;
        }
        if let Some(fragment) = field(message, "content") {
            emit(sink, StreamEvent::Text(fragment.to_string()))?;
        }
        if let Some(calls) = message
            .and_then(|m| m.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                emit(sink, StreamEvent::ToolCall(tool_call(call, tool_calls)?))?;
                tool_calls += 1;
            }
        }

        if object.get("done").and_then(Value::as_bool).unwrap_or(false) {
            let stop = stop_reason(
                object.get("done_reason").and_then(Value::as_str),
                tool_calls > 0,
            );
            emit(
                sink,
                StreamEvent::End {
                    stop,
                    usage: usage(&object),
                },
            )?;
            ended = true;
        }
    }

    if !ended {
        // The body ran out before any object said `done`. Whatever text
        // reached the caller is a fragment, and the contract is that a
        // fragment is an error rather than a short answer.
        return Err(BackendError::Network(
            "the reply ended before the model was finished".to_string(),
        ));
    }

    Ok(())
}

/// A non-empty string field of `message`. Empty fragments are dropped
/// rather than forwarded: every increment of an Ollama stream carries a
/// `content` key whether or not there is anything in it, and an event
/// per empty string would be noise the receiver has to filter anyway.
fn field<'a>(message: Option<&'a Value>, key: &str) -> Option<&'a str> {
    message
        .and_then(|message| message.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// One entry of `message.tool_calls`.
fn tool_call(value: &Value, index: usize) -> Result<ToolCall, BackendError> {
    let function = value
        .get("function")
        .ok_or_else(|| BackendError::Protocol("a tool call has no `function`".to_string()))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError::Protocol("a tool call has no name".to_string()))?;

    let input = match function.get("arguments") {
        // No arguments at all is a call to a tool that takes none.
        None | Some(Value::Null) => Value::Object(Map::new()),
        // Some models hand the arguments over as a JSON string instead
        // of an object. Parsing it here is the difference between a
        // working tool call and a failed turn; a string that is not JSON
        // is the failure the contract names.
        Some(Value::String(text)) if text.trim().is_empty() => Value::Object(Map::new()),
        Some(Value::String(text)) => serde_json::from_str(text).map_err(|err| {
            BackendError::Protocol(format!(
                "the arguments of tool `{name}` are not JSON: {err}"
            ))
        })?,
        Some(other) => other.clone(),
    };

    // The server's own id when it sent one — it is the caller's to echo
    // back, so it travels uninterpreted. Otherwise one is minted, with
    // the tool name in it: ids are numbered per turn, so two turns can
    // each produce a call number 0, and a history encoded afterwards
    // must not be able to match the second turn's result to the first
    // turn's tool.
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{name}-{index}"));

    Ok(ToolCall {
        id,
        name: name.to_string(),
        input,
    })
}

/// Why the turn stopped.
fn stop_reason(done_reason: Option<&str>, saw_tool_call: bool) -> StopReason {
    match done_reason {
        // Checked before tool calls: a turn cut off at the token limit
        // is a fragment even if a whole tool call happened to fit in it.
        Some("length") => StopReason::MaxTokens,
        // Ollama reports a turn that ends in a tool call as an ordinary
        // stop, because it does not track that the caller now owes it a
        // result. The contract does, and a caller that has to know which
        // provider answered to work that out is a caller the contract
        // has failed.
        _ if saw_tool_call => StopReason::ToolUse,
        None | Some("stop") => StopReason::EndTurn,
        Some(other) => StopReason::Other(other.to_string()),
    }
}

/// The counters on the final object.
fn usage(object: &Value) -> Usage {
    Usage {
        input_tokens: count(object, "prompt_eval_count"),
        output_tokens: count(object, "eval_count"),
        // Ollama keeps a prompt cache but does not report what it served
        // from it, and zero is how the contract spells "not reported".
        ..Usage::default()
    }
}

fn count(object: &Value, key: &str) -> u32 {
    object
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32
}

/// Hand one event to the receiver, honouring the one way a turn can be
/// cancelled.
fn emit(sink: &mut EventSink<'_>, event: StreamEvent) -> Result<(), BackendError> {
    match sink(event) {
        Flow::Continue => Ok(()),
        Flow::Stop => Err(BackendError::Cancelled),
    }
}

/// An address in any of the shapes a user might write, as one this
/// client can ask `ureq` for.
///
/// The rules are Ollama's own, because the variable is Ollama's: a bare
/// `host`, a `host:port`, a bare `:port`, or a full URL. A path is kept,
/// so a reverse proxy at `http://box/ollama` works.
///
/// A user name and password are *dropped*. This client does not send
/// them, and a host string ends up in error messages, so keeping a
/// password in it would put a secret one failed request away from a log.
fn normalise_host(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_HOST.to_string();
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => ("http".to_string(), trimmed),
    };

    let (authority, path) = match rest.find('/') {
        Some(cut) => (&rest[..cut], rest[cut..].trim_end_matches('/')),
        None => (rest, ""),
    };

    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };

    // `:11434` and `` both mean this machine.
    let host = if authority.is_empty() || authority.starts_with(':') {
        format!("localhost{authority}")
    } else {
        authority.to_string()
    };

    let host = if has_port(&host) {
        host
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };

    format!("{scheme}://{host}{path}")
}

/// Whether a normalised host is this very machine.
///
/// Works on the output of [`normalise_host`], so the scheme and the port
/// are always there and the authority is always the last thing before a
/// path. Anything that is not plainly loopback is treated as remote:
/// a name that happens to resolve to `127.0.0.1` today is a DNS answer,
/// not a promise.
pub fn is_loopback(host: &str) -> bool {
    let rest = host.split_once("://").map(|(_, rest)| rest).unwrap_or(host);
    let authority = rest.split('/').next().unwrap_or(rest);
    let name = match authority.rsplit_once(']') {
        // `[::1]:11434` — the brackets hold the address.
        Some((address, _)) => address.trim_start_matches('[').to_string(),
        None => authority
            .rsplit_once(':')
            .map(|(name, _)| name.to_string())
            .unwrap_or_else(|| authority.to_string()),
    };
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || name == "::1"
        || name == "0:0:0:0:0:0:0:1"
        || name.starts_with("127.")
}

/// Whether an authority already names a port, minding that the colons in
/// `[::1]` are part of the address rather than a port separator.
fn has_port(host: &str) -> bool {
    match host.rsplit_once(']') {
        Some((_, after)) => after.starts_with(':'),
        None => host.contains(':'),
    }
}
