//! Layer 2: what is cut out of anything about to cross the network.
//!
//! This layer does not judge. It matches shapes — armour lines, provider
//! prefixes, header names, the three-segment shape of a JWT, a password
//! sitting between `//` and `@` in a connection string — and it matches
//! them the same way every time, on every payload, whatever the model
//! happens to think about the text around them.
//!
//! That is the entire reason it exists ahead of
//! [`review`](super::review). A model asked "is there a secret in here"
//! is right most of the time, and the times it is wrong are the times
//! that matter, because one miss is on somebody else's server and cannot
//! be recalled. A regex cannot be talked out of `-----BEGIN OPENSSH
//! PRIVATE KEY-----`.
//!
//! **A hit is cut, not flagged.** The value is replaced by a marker that
//! says what was there and why it went, and that marker travels with the
//! payload. The model on the far side therefore reads "an Anthropic API
//! key was removed here" rather than reading nothing at all — which is
//! the difference between an agent that asks the user for the value and
//! an agent that confidently answers the wrong question.
//!
//! No regex engine is involved. Every rule below is a hand-written scan
//! over the bytes: the crate's dependency list is audited one crate at a
//! time, and none of these shapes needs more than a cursor and a pair of
//! character classes.
//!
//! The one rule with judgement in it is the entropy rule, and it is
//! deliberately biased: it removes more than it strictly must, because
//! the cost of an over-redaction is that the user is asked, and the cost
//! of an under-redaction is a leaked key.
//!
//! ## Two things this layer learned the hard way
//!
//! **A marker may not quote the payload.** [`Kind::Labelled`] used to
//! carry the name it found the value under, and the marker printed it —
//! so `auth_token_sk-ant-…: rejected` had its key cut by one rule and
//! put back, in full, by the marker that replaced it. Every variant of
//! [`Kind`] is a constant now, and the `&'static str` is what makes that
//! a guarantee rather than a habit.
//!
//! **A run is not where a credential ends.** Every rule below reads a
//! maximal run of credential characters, and a run stops at the first
//! character that is not one — so a key with a line break, a space or a
//! zero-width character in it was never one run and was therefore
//! invisible to all of them. [`reassembled_credentials`] reads the
//! payload with its wrapping taken out; it looks only for the *named*
//! shapes, because taking the wrapping out of a paragraph of English
//! gives one long run of letters and asking the entropy rule about that
//! would be asking it to redact prose.

use std::fmt;

/// Characters that hold a credential together. Everything else ends one.
///
/// `/`, `+` and `=` are here because base64 uses them, `.` because a JWT
/// is three segments separated by dots, `-` and `_` because every
/// provider prefix in use has them.
const TOKEN: &[u8] = b"-_./+=";

/// Provider prefixes, longest first so `sk-ant-` is recognised as itself
/// rather than as the `sk-` it starts with.
///
/// The third field is the shortest total length that is worth cutting:
/// the literal `sk-` appears in prose, and a rule that redacted it would
/// teach the user to ignore markers.
/// `sk-ant-` asks for fourteen rather than the twenty it used to.
/// `sk-ant-api03-` is thirteen characters of provider before a key's own
/// body begins, so a twenty-character floor was really a floor of seven
/// body characters — and a model that breaks a key after nineteen
/// characters, which is a wrap width a terminal produces, put the first
/// piece under it. Nothing that is not a key is written `sk-ant-` plus
/// seven more.
const PREFIXES: &[(&str, &str, usize)] = &[
    ("sk-ant-", "an Anthropic API key", 14),
    ("github_pat_", "a GitHub personal access token", 24),
    ("ghp_", "a GitHub token", 20),
    ("gho_", "a GitHub OAuth token", 20),
    ("ghu_", "a GitHub token", 20),
    ("ghs_", "a GitHub token", 20),
    ("ghr_", "a GitHub token", 20),
    ("glpat-", "a GitLab personal access token", 20),
    ("xoxb-", "a Slack bot token", 15),
    ("xoxa-", "a Slack token", 15),
    ("xoxp-", "a Slack user token", 15),
    ("xoxr-", "a Slack token", 15),
    ("xoxs-", "a Slack token", 15),
    ("xapp-", "a Slack app-level token", 15),
    ("AIza", "a Google API key", 30),
    ("sk-", "an OpenAI-style API key", 20),
];

/// AWS key ids: a fixed prefix and sixteen upper-case alphanumerics.
const AWS_PREFIXES: &[&str] = &["AKIA", "ASIA", "ABIA", "ACCA"];
const AWS_LEN: usize = 20;

/// Characters that END A FIELD. A prefix behind one of these is at the
/// head of something — a query parameter, an assignment, a path segment
/// — and that is where a credential is written down.
///
/// `:` was on this list and could never match: `:` is not in [`TOKEN`],
/// so a run never contains one, and the character before a prefix inside
/// a run is always one of TOKEN's.
const FIELD_AFTER: &[u8] = b"=/";

/// Characters that JOIN WORDS. The rest of [`TOKEN`] — and a prefix
/// behind one of these is at the head of something only about half the
/// time, because `-`, `_` and `.` are how a compound name, an
/// environment variable and a dotted identifier are built.
///
/// They are here rather than on the list above because a key gets pasted
/// after one of them constantly: `aws_AKIA…` in a variable name,
/// `profile-AKIA…` in a file name, `account.AKIA…` in a log line. What
/// keeps that from swallowing ordinary names is not the list, it is
/// [`JOINER_MIN_PREFIX`].
///
/// This list and the one above are [`TOKEN`] split in two, and they have
/// to stay that way. A character added to TOKEN and to neither of these
/// would silently become a place a prefix cannot be found — which is the
/// hole this pair was written to close, reopened by omission.
const JOINER_AFTER: &[u8] = b"-_.+";

/// How distinctive a prefix has to be before a word joiner counts as an
/// anchor for it.
///
/// `sk-` is two letters and a hyphen, and hyphens are how English builds
/// compound names — `homepage-sk-translation-draft` would be an OpenAI
/// key under a rule that let a three-character prefix follow one, and so
/// would every Slovak locale file in the tree. Four is where the
/// accidents stop: every other prefix in [`PREFIXES`] is four characters
/// or longer and none of them is a shape a hyphenated name reproduces.
///
/// This is the same judgement the third field of [`PREFIXES`] makes, one
/// level up. `sk-` in prose is not a key; `sk-` in a word is not one
/// either.
const JOINER_MIN_PREFIX: usize = 4;

/// Header names whose value is a credential, matched at the start of a
/// line. `x-api-key` is on the list because it is the header this very
/// program sends its own key in.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
];

/// Fragments that, in the name to the left of a `=` or a `:`, mean the
/// value to the right is a secret. Lower-cased before matching.
const SECRET_NAMES: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "pwd",
    "apikey",
    "api_key",
    "api-key",
    "accesskey",
    "access_key",
    "privatekey",
    "private_key",
    "credential",
    "passphrase",
];

/// The shortest run the entropy rule will look at. Below this, a random
/// string and an identifier are not distinguishable and the rule would
/// be doing damage rather than work.
const ENTROPY_MIN_LEN: usize = 32;

/// The shortest line that can be the rest of a value the line above it
/// began — see [`wrapped_tails`].
///
/// Shorter than [`ENTROPY_MIN_LEN`] on purpose, and that is the whole
/// point of the rule: the tail of a wrapped key is exactly the run that
/// is too short to judge on its own. It is not zero because the line
/// under a secret is sometimes an ordinary short word, and a marker over
/// `deployment` is the kind of noise that teaches a user to stop reading
/// markers.
const WRAP_MIN_LEN: usize = 12;

/// Bits per character above which a run is not a word, a path or an
/// identifier. Measured, not guessed: English prose sits near 3.0,
/// filesystem paths near 3.5, base64 of random bytes near 4.5.
const ENTROPY_MIN_BITS: f64 = 3.6;

/// What was removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A PEM private-key block, armour and all.
    PrivateKey,
    /// A credential with a recognisable provider prefix.
    ApiKey(&'static str),
    /// A `Bearer` token.
    Bearer,
    /// The value of a header that carries a credential.
    Header(&'static str),
    /// A JSON web token.
    Jwt,
    /// The password in a `scheme://user:pass@host` connection string.
    ConnectionPassword,
    /// A value written under a name that says it is a secret.
    ///
    /// It carries the [`SECRET_NAMES`] fragment that matched and **not
    /// the name it was found in**, and the `&'static str` is what makes
    /// that a guarantee rather than a habit. A `String` here used to be
    /// the name off the payload, which the marker then quoted — so
    /// `auth_token_sk-ant-…: rejected` had its key cut by the prefix
    /// rule and put back, in full, inside the marker that replaced it.
    /// Every other variant is already a constant. This one now is too,
    /// so there is no byte of anybody's payload that a marker can reach.
    Labelled(&'static str),
    /// A long run with no natural-language profile — the catch-all for
    /// credentials nobody has given a recognisable shape.
    HighEntropy,
}

impl Kind {
    /// What this was, in the words that go in the marker.
    pub fn what(&self) -> String {
        match self {
            Kind::PrivateKey => "a private key block".to_string(),
            Kind::ApiKey(family) => (*family).to_string(),
            Kind::Bearer => "a bearer token".to_string(),
            Kind::Header(name) => format!("the value of the {name} header"),
            Kind::Jwt => "a JSON web token".to_string(),
            Kind::ConnectionPassword => "the password in a connection string".to_string(),
            Kind::Labelled(word) => format!("a value written under a name containing \"{word}\""),
            Kind::HighEntropy => "a long high-entropy string that may be a credential".to_string(),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what())
    }
}

/// One thing that was taken out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    /// Where it started in the text that was scanned. Kept so a caller
    /// can say where, never so a caller can go back and read it.
    pub at: usize,
    /// How many bytes went. The length of a secret is not a secret, and
    /// it is what makes a manifest add up.
    pub bytes: usize,
}

/// Text with the findings taken out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub findings: Vec<Finding>,
}

impl Redacted {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// The marker left where something was cut.
///
/// It names what went and says who took it, because the agent that reads
/// this on the other side has to be able to tell "there was nothing
/// here" from "there was something here and you may not have it". The
/// second one is a reason to ask the user; the first is not.
pub fn marker(kind: &Kind) -> String {
    format!(
        "[[redacted: {} — removed by the local agent before this left the machine]]",
        kind.what()
    )
}

/// Cut every credential shape out of `text`.
///
/// The result is what may be sent. Nothing in it needs the caller to
/// check anything else first: overlapping matches are resolved here, and
/// what comes back is text plus an account of what is missing from it.
pub fn scan(text: &str) -> Redacted {
    // Cut once and lent to the two rules that work line by line. They
    // both used to cut their own, which was two passes over the payload
    // to reach the same answer — and neither could see the line below
    // the one it was on, which is the hole [`wrapped_tails`] and
    // [`value_on_the_line_below`] close.
    let lines = lines(text);

    let mut spans: Vec<Span> = Vec::new();
    pem_blocks(text, &mut spans);
    header_values(&lines, &mut spans);
    labelled_values(&lines, &mut spans);
    bearer_tokens(text, &mut spans);
    connection_passwords(text, &mut spans);
    runs(text, &mut spans);
    // Reads the payload with its wrapping taken out, which is the only
    // way to see a key that was written down in pieces.
    reassembled_credentials(text, &mut spans);
    // Last, because it reaches forward from what the rules above found:
    // a value that a line break cut in half is still one value.
    wrapped_tails(&lines, &mut spans);

    // Earliest first, and where two rules claim the same ground the one
    // that starts earlier — and then the longer of the two — wins. Both
    // would have removed the secret; taking the longer span removes at
    // least as much, which is the direction this whole module errs in.
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));

    let mut out = String::with_capacity(text.len());
    let mut findings = Vec::new();
    let mut cut_to = 0usize;
    for span in spans {
        if span.end <= span.start || span.end <= cut_to {
            continue;
        }
        // Two rules that overlap without either containing the other:
        // A starts first, B starts inside A and ends past it. Dropping B
        // whole — which is what a plain `span.start < cut_to` test does
        // — emitted A's marker and then sent B's tail, which is the
        // second half of a credential, verbatim. It is swallowed into
        // the cut instead, with no second marker: one continuous stretch
        // went, and the finding that named it says how much.
        if span.start < cut_to {
            if let Some(last) = findings.last_mut() {
                let last: &mut Finding = last;
                last.bytes += span.end - cut_to;
            }
            cut_to = span.end;
            continue;
        }
        out.push_str(&text[cut_to..span.start]);
        out.push_str(&marker(&span.kind));
        findings.push(Finding {
            kind: span.kind,
            at: span.start,
            bytes: span.end - span.start,
        });
        cut_to = span.end;
    }
    out.push_str(&text[cut_to..]);

    Redacted {
        text: out,
        findings,
    }
}

/// A stretch of the input that has to go.
struct Span {
    start: usize,
    end: usize,
    kind: Kind,
}

/// `-----BEGIN ... PRIVATE KEY-----` to the end of its armour.
///
/// An unterminated block runs to the end of the text on purpose: a key
/// that was truncated by whatever produced the payload is still a key,
/// and stopping at the missing `END` line would send the interesting
/// half.
fn pem_blocks(text: &str, out: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while let Some(found) = find(bytes, b"-----BEGIN", at) {
        let line_end = line_end_at(text, found);
        let header = &text[found..line_end];
        if !header.contains("PRIVATE KEY") {
            at = line_end;
            continue;
        }
        let end = match find(bytes, b"-----END", line_end) {
            Some(tail) => line_end_at(text, tail),
            None => text.len(),
        };
        out.push(Span {
            start: found,
            end,
            kind: Kind::PrivateKey,
        });
        at = end;
    }
}

/// `Authorization: Bearer …` and its relatives, value only.
///
/// The header NAME is kept. A payload that says an authorization header
/// was present, without saying what it held, is exactly the shape a
/// remote model can reason about safely.
///
/// A header whose line ends at the colon is a FOLDED header — the
/// specification allows the value on the next line, and a program
/// printing headers into a log wraps them the same way. `authorization`
/// is not a word the labelled rule below knows, so nothing else would
/// catch that one.
fn header_values(lines: &[(usize, &str)], out: &mut Vec<Span>) {
    for (index, &(start, line)) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        let Some(header) = SECRET_HEADERS.iter().find(|h| **h == lower) else {
            continue;
        };
        if value.trim().is_empty() {
            if let Some((start, end)) = value_on_the_line_below(lines, index) {
                out.push(Span {
                    start,
                    end,
                    kind: Kind::Header(header),
                });
            }
            continue;
        }
        let value_at = start + indent + name.len() + 1;
        let lead = value.len() - value.trim_start().len();
        let start = value_at + lead;
        let end = start + value.trim().len();
        if end > start {
            out.push(Span {
                start,
                end,
                kind: Kind::Header(header),
            });
        }
    }
}

/// `API_TOKEN=…`, `password: …` — a value whose own name says what it
/// is.
///
/// This is what catches the credentials nobody gave a distinctive
/// prefix: a hex string in a configuration file is indistinguishable
/// from a checksum until you read the name to its left.
///
/// EVERY pair on the line is examined, not only the first. A payload is
/// as likely to be one line of JSON as it is to be a `.env` file, and on
/// `{"user":"michael","api_key":"…"}` the first separator belongs to
/// `user` — so a rule that stopped there would read the one name on the
/// line that says nothing and send the one value that matters.
///
/// And the value does not have to be on the name's own line. See
/// [`value_on_the_line_below`]: a line that ends at its separator has
/// its value under it, and a scan that stopped at the newline read the
/// name, agreed it was a secret, found nothing beside it, and sent the
/// value on the next line whole.
fn labelled_values(lines: &[(usize, &str)], out: &mut Vec<Span>) {
    for (index, &(start, line)) in lines.iter().enumerate() {
        // A comment is prose about the value, not the value.
        if is_comment(line.trim_start()) {
            continue;
        }
        let bytes = line.as_bytes();
        let mut at = 0usize;
        while at < line.len() {
            if bytes[at] != b'=' && bytes[at] != b':' {
                at += 1;
                continue;
            }
            let separator = at;
            at += 1;

            let Some(name) = name_before(line, separator) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            // The fragment, not the name. What goes in the marker is
            // this program's own constant — see [`Kind::Labelled`] —
            // because the name is payload and a marker that quotes the
            // payload is a second copy of it.
            let Some(word) = SECRET_NAMES.iter().find(|s| lower.contains(*s)) else {
                continue;
            };
            match value_after(line, separator) {
                Some((value_at, value_end)) => {
                    out.push(Span {
                        start: start + value_at,
                        end: start + value_end,
                        kind: Kind::Labelled(word),
                    });
                    // Past the value, so a `:` inside a password is not
                    // read as the start of another pair.
                    at = at.max(value_end);
                }
                // Nothing after the separator AT ALL is a line that
                // ended there, and is not the same thing as a value that
                // is empty: `password: ""` says the password is nothing,
                // and the line under it is somebody else's.
                None if line[separator + 1..].trim().is_empty() => {
                    if let Some((begin, end)) = value_on_the_line_below(lines, index) {
                        out.push(Span {
                            start: begin,
                            end,
                            kind: Kind::Labelled(word),
                        });
                    }
                }
                None => {}
            }
        }
    }
}

/// The value on the line under a name whose own line ended at the
/// separator.
///
/// Everything that writes structured text does this sooner or later:
/// JSON pretty-printed with a long value breaks after the colon, a YAML
/// mapping puts the value under the key, and a folded HTTP header is
/// defined that way. The caller has already decided the name is a
/// secret; what is left to decide is only whether the line below is that
/// name's value or the start of something else.
///
/// It is the value unless it is itself a pair. `credentials:` with
/// `user: michael` under it is a mapping, and taking its first field
/// would remove a user name that is not a secret — while leaving the
/// password, which the rule catches on its own line anyway.
///
/// Only the one line below, and only when it is not blank. A value that
/// is two lines further down is a guess, and this rule does not guess;
/// what carries on past the first line is [`wrapped_tails`], which has a
/// stricter test to pass.
///
/// What it costs, said plainly: a message whose line ends in "I forgot
/// the password:" loses the line under it to a marker. That is the trade
/// the same-line rule already makes — `password: it was the one with the
/// horse` goes whole today — and it is the direction the whole module
/// errs in. The user reads the manifest, and the far model is told that
/// something was withheld; the other way round, nobody is told anything.
fn value_on_the_line_below(lines: &[(usize, &str)], index: usize) -> Option<(usize, usize)> {
    let &(start, line) = lines.get(index + 1)?;
    let trimmed = line.trim();
    if trimmed.is_empty() || is_comment(trimmed) || is_pair(trimmed) {
        return None;
    }
    let begin = start + (line.len() - line.trim_start().len());
    Some((begin, begin + trimmed.len()))
}

/// Whether a line is itself a `name: value` pair.
fn is_pair(line: &str) -> bool {
    let bytes = line.as_bytes();
    for at in 0..line.len() {
        if bytes[at] != b'=' && bytes[at] != b':' {
            continue;
        }
        if name_before(line, at).is_none() {
            continue;
        }
        // Base64 padding is not an assignment: `…LXJlYWw==` ends in the
        // one character that also separates a name from a value, and a
        // wrapped value is exactly what this must not mistake for a
        // field of its own.
        if line[at..].bytes().all(|b| b == b'=') {
            continue;
        }
        return true;
    }
    false
}

/// Whether a line is a comment — prose about a value rather than one.
fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with('#') || trimmed.starts_with("//") || trimmed.starts_with(';')
}

/// The name to the left of a separator.
///
/// Walks back over the quote and the spaces that syntax puts between the
/// name and the colon, then over the name itself. `"api_key" :` and
/// `api_key=` therefore give the same answer, which is the point: the
/// name is what decides, and it should not depend on the file format it
/// was written in.
fn name_before(line: &str, separator: usize) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut end = separator;
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t' | b'"' | b'\'') {
        end -= 1;
    }
    let mut begin = end;
    while begin > 0 && is_name(bytes[begin - 1]) {
        begin -= 1;
    }
    (begin < end).then(|| &line[begin..end])
}

/// Where the value after a separator starts and ends.
///
/// A quoted value ends at its closing quote, so a comma or a brace
/// inside a password is part of the password. The quotes themselves
/// stay: they belong to the file's syntax rather than to the secret, and
/// leaving them makes the marker read as a value where the value was.
///
/// An unquoted value runs to the end of the line — `password: two words`
/// is two words — UNLESS there is another pair further along, in which
/// case the line is structured and the field ends where the next one
/// begins. Both errors this can still make are errors in the same
/// direction as the rest of the module: a value that ends early leaves a
/// fragment, and a value that runs long takes some punctuation with it.
fn value_after(line: &str, separator: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut at = separator + 1;
    while at < line.len() && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    if at >= line.len() {
        return None;
    }

    let quote = bytes[at];
    if quote == b'"' || quote == b'\'' {
        let begin = at + 1;
        // An unterminated quote is a truncated line, not a reason to
        // leave the value in: the rest of the line goes.
        if let Some(end) = line[begin..].find(quote as char).map(|o| begin + o) {
            return (end > begin).then_some((begin, end));
        }
        return (line.len() > begin).then_some((begin, line.len()));
    }

    let rest = &line[at..];
    let end = match rest.contains('=') || rest.contains(':') {
        true => rest
            .find([',', ';', '}', ']', '&'])
            .map(|o| at + o)
            .unwrap_or(line.len()),
        false => line.len(),
    };
    let end = at + line[at..end].trim_end().len();
    (end > at).then_some((at, end))
}

/// Whether a byte can be part of a name to the left of a separator.
fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
}

/// A `Bearer` token anywhere, not only in a header line — they get
/// pasted into prose, into shell commands and into error messages.
fn bearer_tokens(text: &str, out: &mut Vec<Span>) {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut at = 0usize;
    while let Some(found) = find(bytes, b"bearer ", at) {
        let value_at = found + "bearer ".len();
        let end = token_end(text.as_bytes(), value_at);
        at = end.max(value_at + 1);
        if end > value_at {
            out.push(Span {
                start: value_at,
                end,
                kind: Kind::Bearer,
            });
        }
    }
}

/// The password in `scheme://user:password@host`.
///
/// Only the password goes. The scheme, the user and the host are how the
/// far side works out what the string was for, and none of them is the
/// part that grants access.
fn connection_passwords(text: &str, out: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while let Some(found) = find(bytes, b"://", at) {
        let authority = found + 3;
        at = authority;
        let mut cursor = authority;
        let mut colon = None;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'@' => break,
                // The authority ended without a `@`: there was no
                // password to find.
                b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' => {
                    cursor = bytes.len();
                    break;
                }
                b':' if colon.is_none() => colon = Some(cursor),
                _ => {}
            }
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'@' {
            continue;
        }
        if let Some(colon) = colon {
            if cursor > colon + 1 {
                out.push(Span {
                    start: colon + 1,
                    end: cursor,
                    kind: Kind::ConnectionPassword,
                });
            }
        }
        at = cursor;
    }
}

/// The rest of a value that a line break cut in half.
///
/// Nothing in a credential says where it ends, and a payload that has
/// been through an eighty-column log, an editor's hard wrap or a mail
/// client has its keys in two pieces. The rules above find the first
/// piece, because the first piece is the one with the name beside it or
/// the prefix at its head. The second piece has neither: on its own it
/// is a short run of no particular shape — under [`ENTROPY_MIN_LEN`],
/// often of only two character classes — so every rule in this module
/// looks at it and passes, and half a key goes to the far side.
///
/// The precondition is what makes reaching forward safe. Something on
/// the line above has ALREADY been judged a secret, and this only
/// decides how far that secret reaches; a bare run of credential
/// characters under one is the rest of it far more often than it is
/// anything else. It is also the direction this module errs in
/// everywhere else.
///
/// One exception, and it is the one shape that says where it ends: a PEM
/// block is delimited by its own armour, and everything after
/// `-----END …-----` belongs to somebody else.
fn wrapped_tails(lines: &[(usize, &str)], spans: &mut [Span]) {
    // Nothing was found, so there is nothing for a line to be the rest
    // of — and the payload with no secrets in it is the common one.
    if spans.is_empty() || lines.is_empty() {
        return;
    }

    // How far the continuation lines under each line reach, worked out
    // once from the bottom up. Without this table every span would walk
    // the lines below it, and a payload that is a page of base64 has a
    // span on every one of those lines — which is quadratic in the size
    // of the payload, on a function that runs on every outgoing turn.
    let mut reach: Vec<Option<usize>> = vec![None; lines.len()];
    for index in (0..lines.len()).rev() {
        let (start, line) = lines[index];
        if !is_wrapped_tail(line) {
            continue;
        }
        reach[index] = Some(
            reach
                .get(index + 1)
                .copied()
                .flatten()
                .unwrap_or(start + line.len()),
        );
    }

    for span in spans.iter_mut() {
        if matches!(span.kind, Kind::PrivateKey) {
            continue;
        }
        let index = line_of(lines, span.end);
        let (start, line) = lines[index];
        // The span has to END this line — trailing blanks aside, since a
        // value that stops before them still stops at the line break.
        // A span that ends where a line begins ended on the line above
        // and took its break with it; the PEM rule is the one that does
        // that, and it says where it ends by itself.
        let Some(rest) = line.get(span.end - start..) else {
            continue;
        };
        if span.end == start || !rest.trim().is_empty() {
            continue;
        }
        if let Some(end) = reach.get(index + 1).copied().flatten() {
            span.end = end;
        }
    }
}

/// Which line the byte at `at` is on.
fn line_of(lines: &[(usize, &str)], at: usize) -> usize {
    lines
        .partition_point(|&(start, _)| start <= at)
        .saturating_sub(1)
}

/// Whether a whole line is the rest of the value above it.
///
/// Strict, because it is reaching past a line break: the line has to be
/// nothing but credential characters from its first column — an indent,
/// a space, a quote or a comma all mean the payload went back to being
/// structured text — it has to be long enough that its disappearance is
/// worth a marker, it has to use more than one character class so that
/// an ordinary word is not swallowed, and any `=` in it has to be
/// base64's padding rather than an assignment.
fn is_wrapped_tail(line: &str) -> bool {
    if line.len() < WRAP_MIN_LEN || !line.bytes().all(is_token) {
        return false;
    }
    if let Some(equals) = line.find('=') {
        if !line[equals..].bytes().all(|b| b == b'=') {
            return false;
        }
    }
    classes(line) >= 2
}

// ------------------------------------------------- the key written in pieces

/// A stretch of credential characters that is not an ordinary word.
///
/// Three or more characters, because one or two carry no evidence either
/// way, and not something [`is_word`] recognises. A payload's carriers
/// are its identifiers, its hashes, its versions, its paths and its
/// keys; its carriers are not its sentences.
#[derive(Clone, Copy)]
struct Carrier {
    start: usize,
    end: usize,
}

/// Whether a run is an ordinary word of a language.
///
/// Letters only — a digit, a hyphen or an underscore in it means
/// something wrote it rather than said it — and a vowel rhythm wide
/// enough to hold English, Polish and a camel-cased identifier, since
/// what is being excluded here is *evidence*, and a word is not evidence
/// of anything.
fn is_word(run: &str) -> bool {
    if run.len() < 3 || !run.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let vowels = run
        .bytes()
        .filter(|b| b"aeiouyAEIOUY".contains(b))
        .count();
    let ratio = vowels as f64 / run.len() as f64;
    (0.15..=0.75).contains(&ratio)
}

/// Every carrier in `text`, in order.
fn carriers(text: &str) -> Vec<Carrier> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if !is_token(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        at = token_end(bytes, start);
        if at - start >= 3 && !is_word(&text[start..at]) {
            out.push(Carrier { start, end: at });
        }
    }
    out
}

/// The carriers' characters end to end, and where each of those
/// characters came from.
///
/// This is the payload with its wrapping removed: the line breaks, the
/// spaces, the punctuation, the words, and the zero-width characters
/// somebody wove through a key are all gone, and what is left is every
/// stretch of the payload that could be part of a credential, in order.
fn without_wrapping(text: &str, carriers: &[Carrier]) -> (String, Vec<usize>) {
    let mut joined = String::new();
    let mut from = Vec::new();
    for carrier in carriers {
        for (offset, byte) in text[carrier.start..carrier.end].bytes().enumerate() {
            joined.push(byte as char);
            from.push(carrier.start + offset);
        }
    }
    (joined, from)
}

/// A credential that only exists once the payload's wrapping is taken
/// out, and where it sits in the payload as written.
///
/// **Why this rule exists.** Every other rule in this module reads a run
/// of credential characters, and a run ends at the first character that
/// is not one. So a key with a line break in it is not one run, it is
/// two — and each half is short, of few character classes, and of no
/// recognisable shape, which is to say invisible. Measured: an
/// Anthropic key of a hundred and eight characters, wrapped at sixteen
/// columns, produced no findings at all and left every one of its seven
/// pieces in the payload, recoverable by deleting the newlines. The same
/// held for spaces instead of line breaks, and for a zero-width space, a
/// soft hyphen, a non-breaking hyphen or a fullwidth low line woven
/// through it — [`is_token`] takes ASCII only, so a single character
/// from another alphabet cut the run before any rule read it.
///
/// **Why only the named shapes.** What is searched for here is a
/// provider prefix, and never the entropy rule. Taking the wrapping out
/// of a paragraph of English gives one long run of letters, and asking
/// the entropy rule about that is asking it to redact prose. A prefix
/// asks a much narrower question — is the literal `sk-ant-` in this
/// payload once its wrapping is gone, at the start or behind a
/// separator — and the answer is no for text that is not carrying one.
///
/// **What it costs when it fires.** The whole of every carrier the
/// credential touches, and, where the gap between two of them holds no
/// letter or digit, the gap as well: the wrap goes with what it was
/// wrapping, so the payload reads as one marker rather than as seven.
fn reassembled_credentials(text: &str, out: &mut Vec<Span>) {
    for chain in chains(text) {
        let (joined, from) = without_wrapping(text, &chain);

        let mut at = 0usize;
        while at < joined.len() {
            let Some((begin, kind)) = credential_in(&joined, at) else {
                break;
            };
            let end = begin + body(&joined[begin..]).max(1);
            at = end;

            // A hit inside one carrier is one the ordinary rules have
            // already read; this rule is only about what several of them
            // spell together.
            let first = from[begin];
            let last = from[end - 1];
            let touched: Vec<&Carrier> = chain
                .iter()
                .filter(|c| c.end > first && c.start <= last)
                .collect();
            if touched.len() < 2 {
                continue;
            }
            // Nothing but wrapping separates the carriers of a chain, so
            // the whole stretch from the first to the last goes as one:
            // the payload reads as a single marker rather than as seven.
            out.push(Span {
                start: crumbs_before(text, touched[0].start),
                end: crumbs_after(text, touched[touched.len() - 1].end),
                kind,
            });
        }
    }
}

/// The carriers of `text`, grouped into stretches that a wrap could have
/// broken apart.
///
/// A group ends where a letter or a digit sits between two carriers,
/// because that is somebody's sentence rather than a line break. Without
/// that boundary the rule reads the whole payload as one string and
/// finds credentials that are not there: measured, `Here is the value
/// the log printed: sk-ant-…0000 — please look at it.` produced a second
/// finding over the `it.` at the end, which the joined reading had glued
/// to the tail of the key.
fn chains(text: &str) -> Vec<Vec<Carrier>> {
    let mut out: Vec<Vec<Carrier>> = Vec::new();
    let mut chain: Vec<Carrier> = Vec::new();
    for carrier in carriers(text) {
        if let Some(previous) = chain.last() {
            let gap = &text[previous.end..carrier.start];
            if gap.bytes().any(|b| b.is_ascii_alphanumeric()) {
                if chain.len() > 1 {
                    out.push(std::mem::take(&mut chain));
                } else {
                    chain.clear();
                }
            }
        }
        chain.push(carrier);
    }
    if chain.len() > 1 {
        out.push(chain);
    }
    out
}

/// How far past `end` the wrap's last crumb reaches.
///
/// A wrap divides by column count, not by meaning, so the last line of a
/// wrapped key is whatever was left over — measured, one character. A
/// run that short is not a [`Carrier`], because one or two characters
/// are evidence of nothing on their own, so the reassembly that found
/// the other hundred and four characters left it sitting there. It is
/// part of what went. Only crumbs are crossed, and only over whitespace
/// and characters from outside ASCII: three characters is a word, and a
/// word after a key belongs to the sentence, not to the key.
fn crumbs_after(text: &str, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut reach = end;
    loop {
        let mut at = reach;
        while at < bytes.len() && !is_token(bytes[at]) {
            if !is_wrap(bytes[at]) {
                return reach;
            }
            at += 1;
        }
        let run_end = token_end(bytes, at);
        if at == reach || at >= bytes.len() || run_end - at > 2 {
            return reach;
        }
        reach = run_end;
    }
}

/// The same, backwards from `start`.
fn crumbs_before(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut reach = start;
    loop {
        let mut at = reach;
        while at > 0 && !is_token(bytes[at - 1]) {
            if !is_wrap(bytes[at - 1]) {
                return reach;
            }
            at -= 1;
        }
        let mut run_start = at;
        while run_start > 0 && is_token(bytes[run_start - 1]) {
            run_start -= 1;
        }
        if at == reach || at == 0 || at - run_start > 2 {
            return reach;
        }
        reach = run_start;
    }
}

/// Whether a byte is the kind of thing a wrap inserts: whitespace, or
/// anything outside ASCII — a zero-width space, a soft hyphen, a
/// non-breaking hyphen. None of them is part of any credential, and all
/// of them end a run before a rule can read it.
fn is_wrap(byte: u8) -> bool {
    byte.is_ascii_whitespace() || !byte.is_ascii()
}

/// Every maximal run of credential characters, judged on its own.
fn runs(text: &str, out: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if !is_token(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        at = token_end(bytes, start);
        // A run that ends in a full stop ended in a sentence. Nothing
        // this rule looks for ends in one, and cutting it would make the
        // marker swallow the punctuation around it.
        let mut end = at;
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }

        let run = &text[start..end];
        // A provider prefix inside the run is looked for before the run
        // is judged as a whole, so that the marker names the thing that
        // was removed. Deciding it the other way round would leave
        // `?key=sk-ant-…` described as "a long high-entropy string",
        // which is true and useless.
        if let Some((offset, kind)) = credential_in(run, 0) {
            out.push(Span {
                start: start + offset,
                end,
                kind,
            });
            continue;
        }
        if let Some((offset, kind)) = classify(run) {
            out.push(Span {
                start: start + offset,
                end,
                kind,
            });
        }
    }
}

/// A provider-shaped credential inside a run, and where it starts.
///
/// The obvious rule — does this run BEGIN with a known prefix — misses
/// the case `docs/supervisor.md` names outright: a token in a URL query
/// string. `?key=sk-ant-…` is one unbroken run of token characters,
/// because `=` holds a run together, so a rule that only looked at the
/// first characters of it would send the key.
///
/// The rule the anchor exists for is unchanged and is the one that
/// matters: A PREFIX IN THE MIDDLE OF A WORD IS NOT A PREFIX, because
/// `whisk-` is not an OpenAI key. What has changed is what counts as the
/// middle of a word. `=` and `/` are not the only characters a key gets
/// pasted behind — `aws_AKIA…`, `profile-AKIA…` and `account.AKIA…` all
/// went out whole under a rule that only knew those two — so the anchor
/// is now every character that holds a run together, split into the two
/// lists above by how much of an alibi each one is.
///
/// The widening is paid for twice, because an anchor that admits more
/// starts has to be stopped from admitting longer ones:
///
/// * [`JOINER_MIN_PREFIX`] keeps the short prefix out of the places a
///   compound name would supply it.
/// * The length a prefix demands is measured over [`is_body`] — the
///   characters a key is actually made of — and not over the rest of the
///   run. This is what tells `sk-telecom.example.com` from a key: the
///   run is twenty-two characters from `sk-`, and the credential-shaped
///   part of it is ten. `/usr/share/locale/sk-SK/LC_MESSAGES/gtk30.mo`
///   is the same mistake with a path instead of a host, and the scanner
///   made it before this line was written.
///
/// What is returned is where the credential STARTS. Where it ends is
/// still the end of the run, because the caller is cutting and cutting
/// wide is this module's bias — `is_body` decides what is measured, not
/// what is taken.
/// `from` is where to start looking and never where to start counting:
/// the anchor rules below still ask what character precedes a position,
/// so resuming a search partway through a run cannot invent an anchor
/// that the run does not have.
fn credential_in(run: &str, from: usize) -> Option<(usize, Kind)> {
    // Every character of a run is ASCII — `is_token` accepts nothing
    // else — so every index here is a character boundary.
    let bytes = run.as_bytes();
    for at in from..run.len() {
        // How distinctive a prefix has to be to begin here. A run start
        // and a field separator ask nothing; a word joiner asks for a
        // prefix no compound name would produce; inside a word there is
        // no prefix long enough, so the position is not one.
        let least_prefix = match at {
            0 => 0,
            _ if FIELD_AFTER.contains(&bytes[at - 1]) => 0,
            _ if JOINER_AFTER.contains(&bytes[at - 1]) => JOINER_MIN_PREFIX,
            _ => continue,
        };

        let tail = &run[at..];
        for (prefix, family, least) in PREFIXES {
            // `body` last: it is the only test here that walks the run,
            // and a prefix that did not match makes it pointless work on
            // a function that runs over every outgoing payload.
            if prefix.len() >= least_prefix && tail.starts_with(prefix) && body(tail) >= *least {
                return Some((at, Kind::ApiKey(family)));
            }
        }
        if AWS_PREFIXES
            .iter()
            .any(|p| p.len() >= least_prefix && tail.starts_with(p))
            && body(tail) >= AWS_LEN
            && tail.as_bytes()[..AWS_LEN]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Some((at, Kind::ApiKey("an AWS access key id")));
        }
    }
    None
}

/// How much of `tail` a credential could be made of.
///
/// Every provider above writes its keys in base64url — alphanumerics,
/// `-` and `_` — and not one of them puts a `.` or a `/` in one. Those
/// two are in [`TOKEN`] so that a JWT keeps its segments and a path
/// stays one run, and they are exactly what lets a hostname or a
/// directory go on reaching for the length a prefix rule asks for long
/// after the credential-shaped part of it ended. So the length test is
/// asked of this and not of the whole run.
///
/// `=` and `+` are excluded for the same reason and cost nothing: base64
/// pads at its end, not inside a provider prefix, and a key is over by
/// the time either turns up.
fn body(tail: &str) -> usize {
    tail.bytes().take_while(|b| is_body(*b)).count()
}

/// Whether a byte can be part of a key with a provider prefix.
fn is_body(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// What a run is, if it is anything, and where in the run that starts —
/// the rules that have no prefix to anchor on.
///
/// The offset exists because of one disguise. A run is judged as a
/// whole, so gluing an English phrase to a secret with hyphens made the
/// whole read as language and the entropy rule declined to speak:
/// `attachment-the-quick-brown-fox-…-carries-<secret>` went out entire.
/// So when the run as a whole is not high-entropy, a window of
/// [`ENTROPY_MIN_LEN`] is walked along it, and the first window that is
/// high-entropy on its own says where the run stopped being language.
/// Everything from there to the end of the run goes — cutting to the end
/// rather than to the end of the window because a credential does not
/// announce its length, and cutting wide is the direction this module
/// errs in everywhere else.
fn classify(run: &str) -> Option<(usize, Kind)> {
    if is_jwt(run) {
        return Some((0, Kind::Jwt));
    }
    entropy_from(run).map(|at| (at, Kind::HighEntropy))
}

/// Where a run stops looking like language, if it ever does.
///
/// A window has a stricter test to pass than the whole run: **one word
/// anywhere in it is enough to say no**, where the whole-run test allows
/// a minority of words. Without that, `/usr/share/locale/sk-SK/
/// LC_MESSAGES/gtk30.mo` loses thirty-seven characters to a marker — the
/// window `re/locale/sk-SK/LC_MESSAGES/gtk3` has four character classes,
/// no prose *majority*, and enough entropy, and it is a path. The
/// windows a credential produces have no words in them at all, because a
/// credential is not made of any.
fn entropy_from(run: &str) -> Option<usize> {
    if run.len() < ENTROPY_MIN_LEN || !run.is_ascii() {
        return None;
    }
    if is_high_entropy(run) {
        return Some(0);
    }
    // Byte indices are character boundaries: `is_ascii` was just
    // checked, and a run is `is_token` bytes, all of which are ASCII.
    (1..=run.len() - ENTROPY_MIN_LEN).find(|at| {
        let window = &run[*at..at + ENTROPY_MIN_LEN];
        is_high_entropy(window) && !holds_a_word(window)
    })
}

/// Whether any stretch of letters in `run` is a word.
fn holds_a_word(run: &str) -> bool {
    run.split(|c: char| !c.is_ascii_alphabetic()).any(is_word)
}

/// Three base64url segments, the first of which is a JSON header.
///
/// `eyJ` is `{"` in base64, so a token that does not start with it is
/// not one of these — which is what keeps this rule from firing on every
/// dotted identifier in a stack trace.
fn is_jwt(run: &str) -> bool {
    if !run.starts_with("eyJ") || run.len() < 30 {
        return false;
    }
    let segments: Vec<&str> = run.split('.').collect();
    if segments.len() < 2 || segments.len() > 3 {
        return false;
    }
    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
    })
}

/// The fewest character classes a run can have and still be judged.
///
/// **It was three, and three was a general-purpose way out.** Hexadecimal
/// is lower-case letters and digits: two classes. So is base32 in upper
/// case. A rule that will not look at a two-class run cannot see
/// `xxd -p` output, cannot see `base32`, and cannot see the credentials
/// several providers actually mint that way — a Twilio auth token is
/// thirty-two lower-case hex characters and a pre-2021 GitHub token is
/// forty.
///
/// **What the third class was buying, and what dropping it costs.** A
/// forty-character hexadecimal digest is a git revision, and it is also
/// a GitHub personal access token, and there is no test that tells them
/// apart — they are the same forty characters. So this is a choice, not
/// a refinement: a commit hash in a payload now gets a marker. The cost
/// of that is a sentence the far model has to ask about. The cost of the
/// other choice is a token on somebody else's server, which cannot be
/// taken back. The module's bias is written down at the top of this file
/// and this is it being paid for.
const ENTROPY_MIN_CLASSES: u32 = 2;

/// A long run that is not a word or a path.
///
/// Three conditions, and each one is carrying a different false positive
/// out of the way:
///
/// * **Two character classes**, for the reasons at
///   [`ENTROPY_MIN_CLASSES`]. One class is a word, and a word is what
///   the prose test below is for.
/// * **No prose profile.** `/home/michael/Documents/annual-report` has
///   three classes and thirty-odd characters. Splitting it at the
///   non-letters gives words with the vowel rhythm of language; a
///   base64 blob gives one long piece that has no rhythm at all.
/// * **Entropy.** The last check rather than the first, because on its
///   own it says almost nothing at these lengths.
fn is_high_entropy(run: &str) -> bool {
    if run.len() < ENTROPY_MIN_LEN || !run.is_ascii() {
        return false;
    }
    if classes(run) < ENTROPY_MIN_CLASSES || looks_like_prose(run) {
        return false;
    }
    entropy_bits(run) >= ENTROPY_MIN_BITS
}

/// How many of the four character classes appear: lower, upper, digit,
/// and the punctuation base64 and the provider prefixes use.
fn classes(run: &str) -> u32 {
    [
        run.bytes().any(|b| b.is_ascii_lowercase()),
        run.bytes().any(|b| b.is_ascii_uppercase()),
        run.bytes().any(|b| b.is_ascii_digit()),
        run.bytes().any(|b| TOKEN.contains(&b)),
    ]
    .into_iter()
    .map(u32::from)
    .sum()
}

/// Whether the run reads like language.
///
/// Two or more pieces with a plausible vowel ratio, covering most of the
/// letters in the run — or one piece that is the whole run, which is a
/// word.
///
/// A lone word used to be excluded, and the reason was good: a word
/// glued to a secret would have excused the secret. What answers that
/// now is the window in [`entropy_from`], which asks the same question
/// again of every thirty-two characters of the run and does not care
/// what the rest of it reads like. With that behind it, this test can
/// afford to recognise `createDefaultConfigurationBuilder` — thirty-two
/// characters, two classes, no separator anywhere in it — as the
/// identifier it is rather than as a credential.
fn looks_like_prose(run: &str) -> bool {
    let mut word_like = 0usize;
    let mut word_letters = 0usize;
    let mut letters = 0usize;
    for piece in run.split(|c: char| !c.is_ascii_alphabetic()) {
        letters += piece.len();
        if piece.len() < 3 {
            continue;
        }
        let vowels = piece
            .bytes()
            .filter(|b| b"aeiouyAEIOUY".contains(b))
            .count();
        let ratio = vowels as f64 / piece.len() as f64;
        if (0.2..=0.6).contains(&ratio) {
            word_like += 1;
            word_letters += piece.len();
        }
    }
    let one_word = word_like == 1 && word_letters == run.len();
    (word_like >= 2 || one_word) && letters > 0 && word_letters * 10 >= letters * 6
}

/// Shannon entropy of the run, in bits per character.
fn entropy_bits(run: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in run.bytes() {
        counts[byte as usize] += 1;
    }
    let total = run.len() as f64;
    let mut bits = 0.0;
    for count in counts {
        if count == 0 {
            continue;
        }
        let p = count as f64 / total;
        bits -= p * p.log2();
    }
    bits
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || TOKEN.contains(&byte)
}

/// Where the run starting at `from` ends.
fn token_end(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && is_token(bytes[end]) {
        end += 1;
    }
    end
}

/// `needle` in `haystack` at or after `from`.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// Every line with the byte offset it starts at.
fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for line in text.split_inclusive('\n') {
        out.push((start, line.trim_end_matches(['\n', '\r'])));
        start += line.len();
    }
    out
}

/// The end of the line the byte at `at` is on.
fn line_end_at(text: &str, at: usize) -> usize {
    text[at..]
        .find('\n')
        .map(|offset| at + offset + 1)
        .unwrap_or(text.len())
}
