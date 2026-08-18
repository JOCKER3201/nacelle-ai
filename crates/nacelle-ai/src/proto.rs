//! Protocol v0 — JSON Lines on the socket, exactly as written in
//! `.gap-program/decyzja-nacelle-ai-daemon.md`. Another fleet is
//! writing the client against that same page, so this module adds
//! NOTHING to the shapes below and reads nothing beyond them.
//!
//! Commands, client → daemon, one per line:
//!
//! ```text
//! {"cmd":"hello","client":"<name>","proto":0}
//! {"cmd":"ask","id":N,"text":"...","backend":"claude"|"local"|"auto"}
//! {"cmd":"tool","id":N,"tool":"loop"|"photo"|"sort","args":{...}}
//! {"cmd":"approve","id":N,"allow":true|false}
//! {"cmd":"cancel","id":N}
//! ```
//!
//! Events, daemon → client:
//!
//! ```text
//! {"ev":"hello","proto":0,"backends":[...]}
//! {"ev":"delta","id":N,"text":"..."}   {"ev":"done","id":N,...}
//! {"ev":"approval","id":N,"desc":"..."}   (waits for cmd:approve)
//! {"ev":"progress","id":N,"msg":"..."}   {"ev":"error","id":N,"msg":"..."}
//! ```
//!
//! `done` may carry more fields than `id` — the spec writes it with an
//! ellipsis — and a client keys on `ev` and `id`, so extras are free to
//! grow. Everything else is closed.
//!
//! **`hello` is a negotiation, not a greeting.** The `proto` a client
//! names is compared against [`SPOKEN`], and a version this daemon does
//! not speak is answered with `ev:error` carrying the list — see
//! [`version_refused`]. `hello` has no `id` of its own, so that error
//! carries `0`, which is the id every id-less answer already uses.

use serde_json::{json, Map, Value};

/// The protocol version both sides say in `hello`.
pub const PROTO: u64 = 0;

/// Every version this daemon can serve, oldest first.
///
/// One entry today, and the list exists for the day there are two: a
/// client that asked for a version not on it is handed the list, so it
/// can pick one it also speaks instead of guessing. A daemon that read
/// the `proto` field and threw it away — which is what this one did
/// until 2026-08-18 — has no way to tell a client the truth about that,
/// and neither side finds out they disagree until an event arrives in a
/// shape the other cannot read.
pub const SPOKEN: &[u64] = &[PROTO];

/// Whether this daemon can serve a client that asked for `proto`.
pub fn speaks(proto: u64) -> bool {
    SPOKEN.contains(&proto)
}

/// The longest client name that is repeated back. A name arrives from
/// the wire and ends up in an error the client reads and in a line on
/// the daemon's stderr, so it is cut to something a person can read at
/// the end of a sentence.
pub const NAME_MAX: usize = 64;

/// How a client is named in a message about it.
///
/// The daemon logs and answers with this rather than the raw field, for
/// two reasons and no others: the log is LINES, so a name carrying a
/// newline would write a line of its own — and the daemon's own lines
/// are the only account anybody has of what it did — and a name is a
/// word, so its length is capped at something that fits in a sentence.
///
/// Dropped along with the control characters: the bidirectional
/// overrides and the zero-width joins. They are not control characters
/// by `char::is_control`, and a name wearing one reorders the rest of
/// the line it lands in. Nothing legible is lost — a widget's name is a
/// word somebody chose.
///
/// A client that did not name itself is said to be one. The four widgets
/// each name themselves, and which of them is on the other end is the
/// whole point of writing this down.
pub fn client_label(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !reorders(*c))
        .take(NAME_MAX)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "a client that did not name itself".to_string()
    } else {
        format!("client \"{cleaned}\"")
    }
}

/// A character that rearranges the text around it rather than being
/// text: the bidirectional embeddings, overrides and isolates, and the
/// zero-width marks and joiners.
fn reorders(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// What a `hello` naming a version this daemon does not speak is
/// answered with — the whole of it, so the sentence that reaches the
/// client is the sentence written here.
pub fn version_refused(client: &str, asked: u64) -> String {
    format!(
        "{client} asked for protocol {asked}, which this daemon does not speak \u{2014} it \
         speaks {}. Say hello again naming one of those; until then an ask or a tool on this \
         connection is refused, because neither side knows what the other means.",
        spoken_list()
    )
}

/// `0`, `0 or 1`, `0, 1 or 2` — the versions, as a sentence says them.
fn spoken_list() -> String {
    let names: Vec<String> = SPOKEN.iter().map(u64::to_string).collect();
    match names.split_last() {
        None => "no version at all".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// Said to an `ask` or `tool` on a connection whose `hello` named a
/// version this daemon does not speak.
pub fn version_pending(asked: u64) -> String {
    format!(
        "this connection said it speaks protocol {asked}; this daemon speaks {} and will not \
         run a command it may be misreading \u{2014} say hello again naming one of those",
        spoken_list()
    )
}

/// One command off the wire.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Hello {
        client: String,
        proto: u64,
    },
    Ask {
        id: u64,
        text: String,
        backend: Wanted,
    },
    Tool {
        id: u64,
        tool: String,
        args: Value,
    },
    Approve {
        id: u64,
        allow: bool,
    },
    Cancel {
        id: u64,
    },
}

/// Which backend an `ask` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Wanted {
    /// The daemon's own order: the local model answers, Claude is for
    /// when the user asks. See `backends` for the policy.
    #[default]
    Auto,
    Claude,
    Local,
}

impl Wanted {
    pub fn of(name: &str) -> Option<Wanted> {
        match name {
            "auto" => Some(Wanted::Auto),
            "claude" => Some(Wanted::Claude),
            "local" => Some(Wanted::Local),
            _ => None,
        }
    }
}

/// A line that was not a command, and the id to answer on when the line
/// carried one — an error event with the caller's own id is the only
/// answer they can match to what they sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Fault {
    pub id: u64,
    pub msg: String,
}

impl Command {
    /// One line, parsed strictly against the shapes above.
    pub fn parse(line: &str) -> Result<Command, Fault> {
        let fault = |id: u64, msg: String| Fault { id, msg };
        let value: Value = serde_json::from_str(line)
            .map_err(|e| fault(0, format!("this line is not JSON: {e}")))?;
        let Some(object) = value.as_object() else {
            return Err(fault(0, "a command is a JSON object".to_string()));
        };
        // Read the id first, whatever else is wrong: an error the
        // client cannot match to a command is an error they can only
        // log.
        let id = object.get("id").and_then(Value::as_u64).unwrap_or(0);
        let Some(cmd) = object.get("cmd").and_then(Value::as_str) else {
            return Err(fault(id, "a command names itself in \"cmd\"".to_string()));
        };
        match cmd {
            // A `hello` with no `proto` at all is read as this version:
            // the field was written into the spec beside the command,
            // so a line without it is a client that never knew there
            // was a version to name, and there has only ever been one.
            // A `proto` that IS there and is not a version number is a
            // different thing — a client meaning something by that
            // field — and guessing at it is exactly what this command
            // exists to stop.
            "hello" => Ok(Command::Hello {
                client: text(object, "client").unwrap_or_default(),
                proto: match object.get("proto") {
                    None => PROTO,
                    Some(value) => value.as_u64().ok_or_else(|| {
                        fault(id, "\"proto\" is a whole number: the protocol version".to_string())
                    })?,
                },
            }),
            "ask" => {
                let id = need_id(object).map_err(|msg| fault(0, msg))?;
                let text = text(object, "text")
                    .ok_or_else(|| fault(id, "ask needs \"text\"".to_string()))?;
                let backend = match object.get("backend") {
                    None => Wanted::Auto,
                    Some(Value::String(name)) => Wanted::of(name).ok_or_else(|| {
                        fault(
                            id,
                            format!(
                                "there is no backend called \"{name}\" — it is auto, claude \
                                 or local"
                            ),
                        )
                    })?,
                    Some(_) => {
                        return Err(fault(id, "\"backend\" is a string".to_string()));
                    }
                };
                Ok(Command::Ask { id, text, backend })
            }
            "tool" => {
                let id = need_id(object).map_err(|msg| fault(0, msg))?;
                let tool = text(object, "tool")
                    .ok_or_else(|| fault(id, "tool needs \"tool\"".to_string()))?;
                let args = object.get("args").cloned().unwrap_or_else(|| json!({}));
                Ok(Command::Tool { id, tool, args })
            }
            "approve" => {
                let id = need_id(object).map_err(|msg| fault(0, msg))?;
                let allow = object
                    .get("allow")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| fault(id, "approve needs \"allow\": true or false".to_string()))?;
                Ok(Command::Approve { id, allow })
            }
            "cancel" => {
                let id = need_id(object).map_err(|msg| fault(0, msg))?;
                Ok(Command::Cancel { id })
            }
            other => Err(fault(id, format!("there is no command called \"{other}\""))),
        }
    }
}

fn text(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_string)
}

fn need_id(object: &Map<String, Value>) -> Result<u64, String> {
    object
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "this command needs a numeric \"id\"".to_string())
}

/// One event for the wire. [`Event::line`] is the bytes to write —
/// one JSON object and the newline that ends it.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Hello { backends: Vec<String> },
    Delta { id: u64, text: String },
    /// The end of an id. `extra` rides along beside `ev` and `id` —
    /// the answer's text, the output path, a cancellation note.
    Done { id: u64, extra: Value },
    Approval { id: u64, desc: String },
    Progress { id: u64, msg: String },
    Error { id: u64, msg: String },
}

impl Event {
    pub fn done(id: u64) -> Event {
        Event::Done {
            id,
            extra: json!({}),
        }
    }

    pub fn value(&self) -> Value {
        match self {
            Event::Hello { backends } => json!({
                "ev": "hello", "proto": PROTO, "backends": backends,
            }),
            Event::Delta { id, text } => json!({ "ev": "delta", "id": id, "text": text }),
            Event::Done { id, extra } => {
                let mut object = Map::new();
                object.insert("ev".to_string(), json!("done"));
                object.insert("id".to_string(), json!(id));
                if let Some(extra) = extra.as_object() {
                    for (k, v) in extra {
                        // `ev` and `id` are the envelope; nothing may
                        // overwrite them.
                        if k != "ev" && k != "id" {
                            object.insert(k.clone(), v.clone());
                        }
                    }
                }
                Value::Object(object)
            }
            Event::Approval { id, desc } => json!({ "ev": "approval", "id": id, "desc": desc }),
            Event::Progress { id, msg } => json!({ "ev": "progress", "id": id, "msg": msg }),
            Event::Error { id, msg } => json!({ "ev": "error", "id": id, "msg": msg }),
        }
    }

    /// The line as written: the object, then `\n`.
    pub fn line(&self) -> String {
        let mut line = self.value().to_string();
        line.push('\n');
        line
    }
}
