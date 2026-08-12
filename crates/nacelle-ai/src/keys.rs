//! What a keystroke means here, decided apart from the window that
//! receives it.
//!
//! Two questions, and neither of them wants a display to answer:
//!
//! * **Who owns this key?** While a change is waiting on the user, Enter
//!   and Escape belong to that question and to nothing else — the worker
//!   thread is blocked on the answer, and a return that sent a second
//!   question instead of answering the first would leave the agent
//!   waiting for a decision the user believes they gave. Every other key
//!   still edits the field, because a person may well type their next
//!   question while they think about the one on screen.
//!
//! * **What did the field do with it?** The field answers in
//!   [`InputEdited`], and the two answers that mean something to the
//!   window are `Submit` and `Cancel`. Escape reaches the field FIRST
//!   and only becomes "stop the answer" when the field says it had
//!   nothing of its own to cancel — an IME composition is cancelled by
//!   the same key, and a half-typed word must not stop the agent.
//!
//! The translation from winit's key set to the toolkit's neutral one
//! lives here too; it is the only part that names the window library.

use nacelle::focus::{Key as FKey, KeyEv, Mods};
use nacelle::object::text_input::{key_msg, InputEdited, InputMsg};

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Who a key belongs to this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Route {
    /// The waiting change takes it.
    Approval(Answer),
    /// The field takes it.
    Field(InputMsg),
    /// Nobody wants it.
    Nobody,
}

/// What the user said about a waiting change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Decline,
}

/// What the window has to do about a key.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// Redraw and no more.
    Redraw,
    /// Send what the field holds.
    Send,
    /// Stop the answer that is arriving.
    Stop,
    /// Put this on the clipboard.
    Copy(String),
    /// Fetch the clipboard and give it back to the field.
    Paste,
}

/// Which of the three takes this key.
pub fn route(ev: &KeyEv, approval_open: bool) -> Route {
    if approval_open {
        match ev.key {
            FKey::Enter => return Route::Approval(Answer::Allow),
            FKey::Escape => return Route::Approval(Answer::Decline),
            _ => {}
        }
    }
    match key_msg(ev) {
        Some(msg) => Route::Field(msg),
        None => Route::Nobody,
    }
}

/// What the field's answer means to the window.
///
/// `busy` is whether an exchange is in flight. Escape on a settled field
/// with nothing running is not an error and not a stop — there is
/// nothing to stop — so it asks only for a redraw.
pub fn act(edited: InputEdited, busy: bool) -> Act {
    match edited {
        InputEdited::Submit => Act::Send,
        InputEdited::Cancel if busy => Act::Stop,
        InputEdited::CopyRequest { text, .. } => Act::Copy(text),
        InputEdited::PasteRequest => Act::Paste,
        _ => Act::Redraw,
    }
}

/// winit's key, as the toolkit's neutral one.
///
/// A key with no neutral name simply produces nothing: the toolkit's own
/// rule, and it means an unknown key is inert rather than mistaken for
/// another. `text` rides along beside the key rather than instead of it,
/// which is what lets a field insert what the platform composed while a
/// chord still matches on the key itself.
pub fn key_ev(key: &Key, text: Option<String>, mods: ModifiersState, repeat: bool) -> Option<KeyEv> {
    let mut m = Mods::NONE;
    if mods.control_key() {
        m = m | Mods::CTRL;
    }
    if mods.shift_key() {
        m = m | Mods::SHIFT;
    }
    if mods.alt_key() {
        m = m | Mods::ALT;
    }
    if mods.super_key() {
        m = m | Mods::SUPER;
    }
    let k = match key {
        Key::Character(s) => FKey::Char(s.chars().next()?),
        Key::Named(n) => match n {
            NamedKey::Enter => FKey::Enter,
            NamedKey::Escape => FKey::Escape,
            NamedKey::Tab => FKey::Tab,
            NamedKey::Backspace => FKey::Backspace,
            NamedKey::Delete => FKey::Delete,
            NamedKey::Space => FKey::Space,
            NamedKey::ArrowLeft => FKey::Left,
            NamedKey::ArrowRight => FKey::Right,
            NamedKey::ArrowUp => FKey::Up,
            NamedKey::ArrowDown => FKey::Down,
            NamedKey::Home => FKey::Home,
            NamedKey::End => FKey::End,
            NamedKey::PageUp => FKey::PageUp,
            NamedKey::PageDown => FKey::PageDown,
            NamedKey::Insert => FKey::Insert,
            NamedKey::ContextMenu => FKey::Menu,
            _ => return None,
        },
        _ => return None,
    };
    Some(KeyEv {
        key: k,
        mods: m,
        repeat,
        text,
    })
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: FKey) -> KeyEv {
        KeyEv {
            key,
            mods: Mods::NONE,
            repeat: false,
            text: None,
        }
    }

    fn chord(key: FKey, mods: Mods) -> KeyEv {
        KeyEv {
            key,
            mods,
            repeat: false,
            text: None,
        }
    }

    #[test]
    fn return_and_escape_belong_to_the_field_while_nothing_is_waiting() {
        assert_eq!(route(&ev(FKey::Enter), false), Route::Field(InputMsg::Enter));
        assert_eq!(
            route(&ev(FKey::Escape), false),
            Route::Field(InputMsg::Escape)
        );
    }

    #[test]
    fn a_waiting_change_takes_return_and_escape_and_leaves_the_rest() {
        assert_eq!(
            route(&ev(FKey::Enter), true),
            Route::Approval(Answer::Allow)
        );
        assert_eq!(
            route(&ev(FKey::Escape), true),
            Route::Approval(Answer::Decline)
        );
        // Typing the next question while deciding is still typing.
        assert_eq!(
            route(&ev(FKey::Char('a')), true),
            Route::Field(InputMsg::Insert("a".into()))
        );
    }

    #[test]
    fn a_key_this_window_has_no_name_for_belongs_to_nobody() {
        assert_eq!(route(&ev(FKey::F(7)), false), Route::Nobody);
        assert_eq!(route(&chord(FKey::Char('k'), Mods::CTRL), false), Route::Nobody);
    }

    #[test]
    fn submitting_the_field_sends_the_question() {
        assert_eq!(act(InputEdited::Submit, false), Act::Send);
        assert_eq!(act(InputEdited::Submit, true), Act::Send);
    }

    #[test]
    fn escape_stops_the_answer_only_while_one_is_arriving() {
        assert_eq!(act(InputEdited::Cancel, true), Act::Stop);
        assert_eq!(act(InputEdited::Cancel, false), Act::Redraw);
    }

    #[test]
    fn an_edit_that_the_field_absorbed_asks_for_nothing_but_a_frame() {
        for edited in [
            InputEdited::None,
            InputEdited::Moved,
            InputEdited::Edited,
            InputEdited::Rejected,
        ] {
            assert_eq!(act(edited, true), Act::Redraw);
        }
    }

    #[test]
    fn the_clipboard_intents_leave_the_field_as_the_window_s_work() {
        assert_eq!(
            act(
                InputEdited::CopyRequest {
                    text: "abc".into(),
                    cut: false
                },
                false
            ),
            Act::Copy("abc".into())
        );
        assert_eq!(act(InputEdited::PasteRequest, false), Act::Paste);
    }

    #[test]
    fn winit_keys_arrive_as_the_toolkit_s_neutral_ones() {
        let mods = ModifiersState::empty();
        let enter = key_ev(&Key::Named(NamedKey::Enter), None, mods, false).unwrap();
        assert_eq!(enter.key, FKey::Enter);
        assert_eq!(enter.mods, Mods::NONE);
        let typed = key_ev(
            &Key::Character("q".into()),
            Some("q".into()),
            mods,
            false,
        )
        .unwrap();
        assert_eq!(typed.key, FKey::Char('q'));
        assert_eq!(typed.text.as_deref(), Some("q"));
        // No neutral name, no event — an unknown key is inert.
        assert!(key_ev(&Key::Named(NamedKey::F13), None, mods, false).is_none());
    }
}
