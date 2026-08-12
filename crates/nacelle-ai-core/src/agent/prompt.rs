//! The system prompt: who the agent is, and what it is looking at.
//!
//! Two properties matter more than the wording.
//!
//! **It is built, not written down.** What the agent knows about the
//! machine — which themes, layouts and addons are installed — comes
//! from the tool registry, because the registry is what actually looked.
//! A list kept here would describe the developer's machine to every
//! user, and would be wrong the first time somebody installed anything.
//!
//! **It does not move.** The text is rendered once when a session
//! starts and reused byte for byte on every turn of that session. That
//! is what a provider's prompt cache needs: the cached prefix is
//! matched by exact bytes, so a prompt carrying a clock, a turn counter
//! or a `HashMap`'s iteration order is a prompt that is never cached
//! and always paid for in full. When the machine changes underneath a
//! session, the session is restarted — which is also the honest thing
//! to do, since the conversation up to that point was had with the old
//! description.

use super::registry::EnvironmentFact;

/// What the agent is, and the rules it works under.
///
/// The two paragraphs about changes are here rather than left to the
/// interface because they describe something the model cannot observe:
/// that its tool calls are gated, and that a refusal is the user
/// speaking. A model that does not know this reads a denial as a
/// malfunction and retries it.
const PREAMBLE: &str = "\
You are the agent built into nacelle, a desktop environment. You run on the user's \
own machine, beside the desktop you are being asked about, and you are talking to \
the person sitting in front of it.

Work from what your tools tell you. The description below was taken from this \
machine when this conversation started; if you need anything that is not in it, \
call a tool rather than guessing, and say you do not know rather than inventing a \
theme, a layout or a setting that may not exist.

Anything that changes the machine is the user's decision, not yours. When you call \
a tool that would change something, the user is shown what it would do and answers \
before it runs. If they decline, that is an answer and not a fault: say what you \
would do differently, or ask what they would prefer. Never call the same tool again \
hoping for a different reply.

Answer in the environment's own vocabulary — themes, layouts, addons, panels — and \
keep it short. The person reading this is looking at a panel on their desktop, not \
at a document.";

/// The closing note, which exists so the model can explain staleness
/// instead of being confused by it: a user who installs a theme and
/// then asks about it should be told to restart the session, not told
/// the theme does not exist.
const FOOTER: &str = "\
This description was taken when the conversation started and is not refreshed while \
it runs. If the user has installed or removed something since, say so and offer to \
start a new conversation.";

/// Render the system prompt for a session.
///
/// `role` is whatever the caller wants this particular agent to be on
/// top of the above — the widget in the corner of a desktop and a
/// standalone window are the same agent with different manners.
pub fn build(role: Option<&str>, facts: &[EnvironmentFact]) -> String {
    let mut prompt = String::with_capacity(PREAMBLE.len() + 512);
    prompt.push_str(PREAMBLE);

    if let Some(role) = role {
        let role = role.trim();
        if !role.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(role);
        }
    }

    if !facts.is_empty() {
        prompt.push_str("\n\n# This machine\n");
        for fact in facts {
            prompt.push_str("\n## ");
            prompt.push_str(&fact.topic);
            prompt.push('\n');

            if let Some(note) = &fact.note {
                prompt.push_str(note);
                prompt.push('\n');
            }

            if fact.items.is_empty() {
                // Said outright, because "nothing here" and "I forgot to
                // look" read identically as an absence, and the model
                // acts differently on them.
                prompt.push_str("(none)\n");
            } else {
                for item in &fact.items {
                    prompt.push_str("- ");
                    prompt.push_str(item);
                    prompt.push('\n');
                }
            }
        }
    }

    prompt.push('\n');
    prompt.push_str(FOOTER);
    prompt
}
