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

use serde_json::{json, Map, Value};

/// The protocol version both sides say in `hello`.
pub const PROTO: u64 = 0;

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
            "hello" => Ok(Command::Hello {
                client: text(object, "client").unwrap_or_default(),
                proto: object.get("proto").and_then(Value::as_u64).unwrap_or(PROTO),
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
