//! The window: winit for events, nacelle-renderer for frames, and the
//! toolkit for everything between them.
//!
//! # Waking up because something far away changed
//!
//! This is the first thing in the project that has to redraw because
//! something OFF this thread moved. The agent loop blocks — a turn owns
//! a worker thread from the first byte of the request to the last byte
//! of the stream — and reports through a [`std::sync::mpsc`] channel. An
//! event loop sitting in `ControlFlow::Wait` on that arrangement would
//! sleep through the whole answer; one spinning on `ControlFlow::Poll`
//! to drain the channel would burn a core discovering, sixty times a
//! second, that nothing had arrived.
//!
//! So the channel is JOINED to the event loop rather than polled beside
//! it. One relay thread does nothing but block on `Receiver::recv` and
//! hand each event to
//! [`winit::event_loop::EventLoopProxy::send_event`]; winit wakes the
//! loop with an `Event::UserEvent` carrying it. The loop sleeps until
//! there is something to do, every fragment of the reply is a wake-up,
//! and the relay costs one stack and no CPU — it is asleep in `recv`
//! for as long as the model is thinking.
//!
//! The relay exists because [`Worker`] hands out a `Receiver` and
//! nothing else: it has no way to be told "and poke this when you send".
//! A waker beside the sender would delete this thread; re-implementing
//! the worker here to avoid it would cost far more than the thread.
//!
//! # Frames
//!
//! A frame is drawn when something changed, not on a clock. Two things
//! keep moving on their own after the last event — the caret's blink and
//! the scrollbar's fade — so the loop asks to be woken again only while
//! one of them is running, and the caret's next wake is computed from
//! `motion.caret_blink` instead of from a frame rate: two wake-ups a
//! second rather than sixty.

use std::sync::mpsc::Receiver;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nacelle::clipboard::{self, Board};
use nacelle::draw::DrawList;
use nacelle::focus::FocusId;
use nacelle::font::FontSystem;
use nacelle::object::text_input::{self, InputModel, InputStyle};
use nacelle::theme::{self, TokenId};
use nacelle::ui::{self, Align, BadgeStyle};
use nacelle::view::hits::{Hit, Hits};
use nacelle::view::list::{self, ListState, ListStyle, ListView};
use nacelle::view::model::RowModel;
use nacelle::view::paint;
use nacelle::view::scroll::{ScrollPhysics, ScrollbarLook};
use nacelle::view::surface::{CtxSurface, Surface};
use nacelle::view::virt;
use nacelle::{Ctx, Rect};

use nacelle_ai::{AgentEvent, PendingApproval, PendingDisclosure, Worker};

use winit::event::{ElementState, Event, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause,
                   WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder};
use winit::keyboard::ModifiersState;
use winit::window::WindowBuilder;

use crate::choice::Indicator;
use crate::conversation::{Conversation, Notice, RowCache};
use crate::keys::{self, Act, Answer, Route};

/// The pace of a frame while something is still moving of its own
/// accord. Not a look — no theme has an opinion on it — but the clock,
/// in the same sense as the toolkit's own longest-step guard: a settle
/// and a fade are smooth at sixty and indistinguishable above it, and
/// this window is asleep the rest of the time anyway.
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// How far from the bottom still counts as being at the bottom.
///
/// Arithmetic, not a look: a difference under one device pixel cannot be
/// seen, and if it counted as "the reader scrolled away" a reply would
/// stop following its own tail over a rounding error.
const AT_BOTTOM_PX: f32 = 1.0;

/// What a window manager shows, and what the program is called.
const TITLE: &str = "nacelle-ai";

fn tok(cell: &'static OnceLock<TokenId>, name: &'static str) -> TokenId {
    *cell.get_or_init(|| theme::id(name).unwrap_or(TokenId::MISSING))
}

/// The identifier of the one control this window has. There is exactly
/// one, so it is focused by construction and the toolkit's focus chain
/// is not built at all — see `InputStyle::focused_fallback`.
fn field_id() -> FocusId {
    FocusId::of("nacelle-ai.question")
}

/// Everything one window remembers between frames.
struct State {
    conversation: Conversation,
    rows: RowCache,
    list: ListState,
    hits: Hits,
    input: InputModel,
    /// The change waiting on the user. Held rather than answered on the
    /// spot: dropping one is a refusal, and a refusal is a decision
    /// nobody made.
    pending: Option<PendingApproval>,
    /// Layer 4: what is about to leave the machine, waiting on the same
    /// answer. Never open at the same time as `pending` — the worker is
    /// blocked on whichever it asked — and dropped rather than answered
    /// when the user stops the turn, which the core reads as a no.
    manifest: Option<PendingDisclosure>,
    worker: Option<Worker>,
    /// The window clock, in seconds since the first frame.
    start: Instant,
    /// When the caret's blink last restarted. The field restarts it on
    /// every edit and keeps that time to itself, so the same rule is
    /// kept here — not to draw the caret, but to know when the next
    /// frame has to happen.
    caret_from: f64,
    mouse: (f32, f32),
    mods: ModifiersState,
    /// Whether the pointer is over the conversation — what a wheel notch
    /// acts on.
    over_list: bool,
    /// The pointer's business with the field.
    field: FieldPointer,
}

/// What the pointer wants of the text field, held between the press and
/// the frame that can answer it.
///
/// A press arrives with an x and nothing else. Turning that into a caret
/// position means measuring the text the field is showing, and measuring
/// needs the glyph atlas — which only exists inside a frame. So the press
/// is RECORDED here and resolved in [`draw`] through
/// [`text_input::hit`], the toolkit's own answer to the question: the
/// field's padding, its horizontal scroll and its mask are its business,
/// and a window that measured for itself would get all three wrong the
/// day any of them changed.
struct FieldPointer {
    /// Where the field was drawn last frame. A press has no geometry of
    /// its own, and the only rectangle that is true is the one on screen.
    r: Rect,
    /// The x the caret is owed, and whether it extends the selection.
    want: Option<(f32, bool)>,
    /// The button went down in the field and has not come up. Latched,
    /// because a drag that wanders out of the field is still the same
    /// drag — releasing the selection the moment the pointer crossed the
    /// edge would make selecting the last word impossible.
    holding: bool,
}

impl FieldPointer {
    /// Before the first frame there is no field on screen, so the
    /// rectangle nothing can be inside is the honest starting point.
    fn new() -> FieldPointer {
        FieldPointer {
            r: Rect::new(0.0, 0.0, 0.0, 0.0),
            want: None,
            holding: false,
        }
    }

    /// A press. Answers whether the field took it, so the caller knows
    /// not to offer it to the conversation as well.
    fn press(&mut self, x: f32, y: f32, extend: bool) -> bool {
        if !self.r.contains(x, y) {
            return false;
        }
        self.want = Some((x, extend));
        self.holding = true;
        true
    }

    /// The pointer moved. Only a held pointer means anything: this is
    /// how a drag selects, and it extends by definition — the anchor was
    /// set by the press.
    fn moved(&mut self, x: f32) {
        if self.holding {
            self.want = Some((x, true));
        }
    }

    fn release(&mut self) {
        self.holding = false;
    }

    /// The caret placement owed, taken exactly once.
    fn take(&mut self) -> Option<(f32, bool)> {
        self.want.take()
    }
}

impl State {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Whether there is anything to ask at all.
    fn can_ask(&self) -> bool {
        self.worker.is_some()
    }
}

/// What arrives from the worker thread while the window is drawing.
///
/// Two channels rather than one, joined here, because they come from
/// different depths: the agent loop reports what it is doing, and the
/// seal — further down, inside the backend, with the request already
/// built — asks whether the user will let it go. A manifest is not an
/// [`AgentEvent`] and putting it in that enum would mean every reader of
/// agent events has to know about layer 4.
enum Incoming {
    Agent(AgentEvent),
    Manifest(PendingDisclosure),
}

/// Opens the window and runs until it closes.
///
/// `manifests` is layer 4's end of the line. It stays silent for a
/// session that never reaches off the machine — which, by default, is
/// every session — and dropping it is what tells the core there is
/// nobody left to ask.
pub fn run(
    indicator: Indicator,
    notices: Vec<Notice>,
    worker: Option<(Worker, Receiver<AgentEvent>)>,
    manifests: Receiver<PendingDisclosure>,
) -> Result<(), String> {
    // The theme, before anything is measured or drawn. It always
    // succeeds — a missing or broken theme degrades to the master
    // compiled into the toolkit — but what it could not read is worth
    // saying out loud rather than leaving as a look nobody asked for.
    for warning in theme::load().warnings.iter() {
        eprintln!("{TITLE}: {warning}");
    }

    let mut fonts = FontSystem::new();

    // A user event carries one thing from the worker thread, so the
    // reply reaches the loop through winit's own queue and needs no
    // polling beside it.
    let event_loop = EventLoopBuilder::<Incoming>::with_user_event()
        .build()
        .map_err(|err| format!("cannot create an event loop: {err}"))?;

    // The one length on screen that does not come from a token, because
    // there is none to come from: the theme describes what is INSIDE a
    // window and has nothing to say about how large the window manager
    // should open one. Everything drawn within it is derived from the
    // window's actual size at the time, so this decides nothing but the
    // first frame.
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(winit::dpi::LogicalSize::new(900.0, 700.0))
        .build(&event_loop)
        .map_err(|err| format!("cannot create a window: {err}"))?;
    // Typing is what this window is for, so the platform's input method
    // is asked for from the start.
    window.set_ime_allowed(true);

    // The engine bakes every u-derived length from the window height, so
    // it has to be told what that is — here and on every resize, never
    // per frame.
    let size = window.inner_size();
    theme::set_viewport(size.height as f32, 1.0);

    let mut gfx = nacelle_renderer::Gfx::new(&window, size.width, size.height);
    let mut dl = DrawList::new();

    let (worker, inbox) = match worker {
        Some((worker, inbox)) => (Some(worker), Some(inbox)),
        None => (None, None),
    };

    // The relay: the whole reason this window can sleep. See the module
    // header. It ends when the agent stops or the loop goes away.
    if let Some(inbox) = inbox {
        let proxy = event_loop.create_proxy();
        std::thread::Builder::new()
            .name("nacelle-ai-relay".into())
            .spawn(move || {
                for event in inbox {
                    if proxy.send_event(Incoming::Agent(event)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|err| format!("cannot start the event relay: {err}"))?;
    }

    // The same again for layer 4. A thread of its own rather than a
    // poll in the frame: the worker is blocked on the answer, so the
    // question has to reach the screen on the strength of its own
    // arrival and not on the strength of something else redrawing.
    {
        let proxy = event_loop.create_proxy();
        std::thread::Builder::new()
            .name("nacelle-ai-manifests".into())
            .spawn(move || {
                for pending in manifests {
                    if proxy.send_event(Incoming::Manifest(pending)).is_err() {
                        // Nobody to show it to any more. Dropping the
                        // request is a refusal, which is the only safe
                        // way for this to end.
                        break;
                    }
                }
            })
            .map_err(|err| format!("cannot start the manifest relay: {err}"))?;
    }

    let mut state = State {
        conversation: Conversation::new(),
        rows: RowCache::new(),
        list: ListState::new(),
        hits: Hits::new(),
        input: InputModel::new(),
        pending: None,
        manifest: None,
        worker,
        start: Instant::now(),
        caret_from: 0.0,
        mouse: (-1.0, -1.0),
        mods: ModifiersState::empty(),
        over_list: false,
        field: FieldPointer::new(),
    };
    // Whatever the choice could not do, said before anything is asked.
    for notice in notices {
        state.conversation.note(notice);
    }

    event_loop
        .run(move |event, target| match event {
            Event::UserEvent(incoming) => {
                match incoming {
                    Incoming::Agent(event) => on_agent_event(&mut state, event),
                    Incoming::Manifest(pending) => on_manifest(&mut state, pending),
                }
                window.request_redraw();
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // The caret has a phase to turn over, or the fade has a
                // frame to lose. Nothing else asks for this.
                window.request_redraw();
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => target.exit(),
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                    let size = window.inner_size();
                    theme::set_viewport(size.height as f32, 1.0);
                    gfx.resize();
                    window.request_redraw();
                }
                WindowEvent::ModifiersChanged(m) => state.mods = m.state(),
                WindowEvent::CursorMoved { position, .. } => {
                    state.mouse = (position.x as f32, position.y as f32);
                    if state.list.scroll.dragging() {
                        drag_thumb(&mut state);
                    }
                    state.field.moved(state.mouse.0);
                    window.request_redraw();
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    on_wheel(&mut state, delta);
                    window.request_redraw();
                }
                WindowEvent::MouseInput { state: pressed, button, .. } => {
                    on_click(&mut state, pressed, button);
                    window.request_redraw();
                }
                WindowEvent::Ime(ime) => {
                    on_ime(&mut state, ime);
                    window.request_redraw();
                }
                // A release is not a keystroke here: the field acts on
                // presses, and the platform's repeat arrives as one.
                WindowEvent::KeyboardInput { event: key, .. }
                    if key.state == ElementState::Pressed =>
                {
                    on_key(&mut state, &key);
                    window.request_redraw();
                }
                WindowEvent::RedrawRequested => {
                    let size = window.inner_size();
                    let caret = draw(
                        &mut state,
                        &indicator,
                        &mut dl,
                        &mut fonts,
                        size.width as f32,
                        size.height as f32,
                    );
                    // Where the platform anchors its candidate window.
                    // winit's call takes LOGICAL coordinates; the frame
                    // is drawn in physical ones.
                    if let Some(r) = caret {
                        let scale = window.scale_factor();
                        window.set_ime_cursor_area(
                            winit::dpi::PhysicalPosition::new(r.x as f64, r.y as f64)
                                .to_logical::<f64>(scale),
                            winit::dpi::PhysicalSize::new(r.w as f64, r.h as f64)
                                .to_logical::<f64>(scale),
                        );
                    }
                    // Only the rows the glyph atlas actually touched
                    // travel to the GPU.
                    let atlas = fonts.take_dirty_rows();
                    static CLEAR: OnceLock<TokenId> = OnceLock::new();
                    // The master says in as many words that this token
                    // IS the swapchain clear colour. Alpha is one
                    // because a window has nothing behind it.
                    let clear = theme::resolved().bed(tok(&CLEAR, "surface.void"));
                    gfx.render(
                        size.width,
                        size.height,
                        &dl.verts,
                        &dl.runs,
                        atlas.map(|(y0, rows)| (fonts.atlas.as_slice(), y0, rows)),
                        [clear.r, clear.g, clear.b, 1.0],
                    );
                    match next_wake(&state) {
                        Some(wait) => target
                            .set_control_flow(ControlFlow::WaitUntil(Instant::now() + wait)),
                        None => target.set_control_flow(ControlFlow::Wait),
                    }
                }
                _ => {}
            },
            _ => {}
        })
        .map_err(|err| format!("the event loop ended with an error: {err}"))
}

// ---------------------------------------------------------------------
// events
// ---------------------------------------------------------------------

/// One event from the worker thread.
fn on_agent_event(state: &mut State, event: AgentEvent) {
    if let AgentEvent::Approval(pending) = event {
        state.conversation.note(Notice::Approval {
            summary: pending.change().summary.clone(),
            detail: pending.change().detail.clone().unwrap_or_default(),
        });
        // A second request while one is open cannot happen — the worker
        // is blocked on the answer to the first — and if it ever did,
        // dropping the older one is a refusal, which is the safe half.
        state.pending = Some(pending);
        return;
    }
    state.conversation.apply(&event);
}

/// Layer 4 has something to show before it sends anything.
///
/// The manifest goes on screen as the core wrote it. This window does
/// not summarise it, shorten it or lay it out itself: what it says and
/// how much of it it says are part of the guarantee, and a view that
/// abridged it would be deciding what the user does not need to know.
fn on_manifest(state: &mut State, pending: PendingDisclosure) {
    state.conversation.note(Notice::Manifest {
        detail: pending.manifest().render(),
    });
    state.manifest = Some(pending);
}

fn on_key(state: &mut State, key: &KeyEvent) {
    let text = key.text.as_ref().map(|t| t.to_string());
    let Some(ev) = keys::key_ev(&key.logical_key, text, state.mods, key.repeat) else {
        return;
    };
    match keys::route(&ev, state.pending.is_some() || state.manifest.is_some()) {
        Route::Approval(answer) => {
            // The manifest first. Only one of the two can be open — the
            // worker is blocked on whichever it asked — and taking this
            // one first means an answer can never be given to the wrong
            // question.
            if let Some(pending) = state.manifest.take() {
                match answer {
                    Answer::Allow => pending.send(),
                    // The reason reaches the model as well as the
                    // screen: a model told only "no" proposes the same
                    // escalation again.
                    Answer::Decline => pending.refuse(
                        "the user read the manifest and would not have this leave the machine",
                    ),
                }
                return;
            }
            let Some(pending) = state.pending.take() else {
                return;
            };
            match answer {
                Answer::Allow => pending.allow(),
                // The words reach the model as that tool's result, so
                // they are written for it: it should propose something
                // else rather than retry the same call.
                Answer::Decline => pending
                    .deny("the user declined this change; propose something else instead"),
            }
        }
        Route::Field(msg) => {
            let edited = state.input.apply(msg);
            // The field restarts its caret blink on every edit; the
            // wake-up schedule restarts with it, or the next wake lands
            // mid-phase and the caret stutters.
            state.caret_from = state.now();
            match keys::act(edited, state.conversation.is_busy()) {
                Act::Send => send(state),
                Act::Stop => {
                    if let Some(worker) = &state.worker {
                        worker.cancel();
                    }
                    // A manifest on screen has a worker blocked behind
                    // it, and a stop that left it there would look like
                    // a program that had stopped listening. Dropping it
                    // is the refusal the core is written to read.
                    state.manifest = None;
                }
                Act::Copy(text) => clipboard::store(Board::Clipboard, &text),
                Act::Paste => {
                    if let Some(text) = clipboard::load(Board::Clipboard) {
                        state.input.apply(text_input::InputMsg::Insert(text));
                    }
                }
                Act::Redraw => {}
            }
        }
        Route::Nobody => {}
    }
}

/// Sends what the field holds, and empties it.
fn send(state: &mut State) {
    let question = state.input.value().trim().to_string();
    if question.is_empty() {
        return;
    }
    let Some(worker) = &state.worker else {
        // The field is drawn disabled without a backend, so this is only
        // reachable if that ever stops being true. Saying nothing would
        // look like a program that swallowed the question.
        state.conversation.note(Notice::NoBackend);
        return;
    };
    match worker.ask(question.clone()) {
        Ok(_) => {
            state.conversation.asked(&question);
            state.input.set_value("");
        }
        Err(err) => state.conversation.note(Notice::Failed {
            detail: format!("{err} — nothing can be asked any more; restart the program."),
        }),
    }
}

fn on_ime(state: &mut State, ime: Ime) {
    let msg = match ime {
        Ime::Preedit(text, range) => text_input::InputMsg::Preedit(text, range),
        Ime::Commit(text) => text_input::InputMsg::Insert(text),
        Ime::Disabled => text_input::InputMsg::PreeditEnd,
        Ime::Enabled => return,
    };
    state.input.apply(msg);
    state.caret_from = state.now();
}

fn on_wheel(state: &mut State, delta: MouseScrollDelta) {
    if !state.over_list {
        return;
    }
    // The reader has taken the offset. Said before the move rather than
    // after it: the frame that follows would otherwise put the view
    // straight back at the bottom and the wheel would do nothing at all.
    // The frame re-arms it if the move ended up at the bottom anyway.
    state.conversation.set_follows_tail(false);
    let physics = ScrollPhysics::from_theme();
    // Positive notches scroll toward the END of the content; every
    // platform spells a wheel-up as a positive delta.
    let notches = match delta {
        MouseScrollDelta::LineDelta(_, y) => -y,
        MouseScrollDelta::PixelDelta(p) => {
            if physics.wheel_px > 0.0 {
                -(p.y as f32) / physics.wheel_px
            } else {
                0.0
            }
        }
    };
    state.list.scroll.wheel(notches, &physics, state.now());
}

fn on_click(state: &mut State, pressed: ElementState, button: MouseButton) {
    if button != MouseButton::Left {
        return;
    }
    if pressed == ElementState::Released {
        state.list.scroll.release();
        state.field.release();
        return;
    }
    let (x, y) = state.mouse;
    // The field first: it is the one thing on screen that is not in the
    // hit list, because it draws itself rather than through a view.
    // Shift makes the press extend the selection instead of dropping it,
    // which is the same chord the field's own Shift+arrows use.
    if state.field.press(x, y, state.mods.shift_key()) {
        return;
    }
    // The bar the last frame drew: the thumb takes the press, the track
    // beside it pages toward the click. Either way the reader now owns
    // the offset — see [`on_wheel`].
    match state.hits.find(x, y) {
        Some((rect, Hit::Thumb { .. })) => {
            state.conversation.set_follows_tail(false);
            state.list.scroll.press_thumb(y, rect);
        }
        Some((_, Hit::Track { toward_end, .. })) => {
            let toward_end = *toward_end;
            let viewport = state.list.extent.viewport;
            state.conversation.set_follows_tail(false);
            state.list.scroll.page(toward_end, viewport, state.now());
        }
        _ => {}
    }
}

/// The pointer moved with the thumb held. The extent the last frame
/// recorded is what the drag is measured against — the view knows where
/// it put the bar, and nothing else does.
fn drag_thumb(state: &mut State) {
    let extent = state.list.extent;
    let Some((track, _)) = extent.bar else { return };
    state
        .list
        .scroll
        .drag(state.mouse.1, extent.viewport, extent.content, track);
}

// ---------------------------------------------------------------------
// the frame
// ---------------------------------------------------------------------

/// Draws one frame; answers with the caret's rectangle when one is on
/// screen, which is where the platform anchors its IME window.
fn draw(
    state: &mut State,
    indicator: &Indicator,
    dl: &mut DrawList,
    fonts: &mut FontSystem,
    w: f32,
    h: f32,
) -> Option<Rect> {
    static BAR_H: OnceLock<TokenId> = OnceLock::new();
    static PAGE_PAD: OnceLock<TokenId> = OnceLock::new();
    static FIELD_H: OnceLock<TokenId> = OnceLock::new();
    static SPLIT: OnceLock<TokenId> = OnceLock::new();
    static ROW_H: OnceLock<TokenId> = OnceLock::new();
    static ROW_GAP: OnceLock<TokenId> = OnceLock::new();

    let now = state.now();
    fonts.begin_frame();
    dl.clear();
    state.hits.clear();

    let t = theme::resolved();
    let bar_h = t.px(tok(&BAR_H, "topbar.h"));
    let pad = t.px(tok(&PAGE_PAD, "panel.content_pad"));
    let field_h = t.px(tok(&FIELD_H, "field.h"));
    // What separates the conversation from the field it is answered in:
    // the ladder's "between groups", because that is what they are.
    let split = t.px(tok(&SPLIT, "space.6"));
    // The pitch `view::list` scrolls in. Read here so the tail offset
    // below is computed against the very number the list uses.
    let pitch = (t.px(tok(&ROW_H, "list.row_h")) + t.px(tok(&ROW_GAP, "list.gap"))).max(1.0);

    let bar_r = Rect::new(0.0, 0.0, w, bar_h);
    let body = Rect::new(
        pad,
        bar_h + pad,
        (w - 2.0 * pad).max(1.0),
        (h - bar_h - 2.0 * pad).max(1.0),
    );
    let field_r = Rect::new(body.x, body.bottom() - field_h, body.w, field_h);
    let list_r = Rect::new(body.x, body.y, body.w, (body.h - field_h - split).max(1.0));

    state.over_list = list_r.contains(state.mouse.0, state.mouse.1);
    state.field.r = field_r;

    let mut ctx = Ctx {
        dl,
        fonts,
        w,
        h,
        t: now,
        mouse: state.mouse,
        term_font_scale: 1.0,
        ui_font_scale: 1.0,
        panel_scale: 1.0,
        // One control, focused by construction: a chain of one has
        // nothing to navigate and no Tab order to keep.
        focus: None,
        // Nothing here is trimmed that a second reading would rescue:
        // a body wraps rather than ellipsises, and a headline is a
        // speaker's name.
        tips: None,
    };

    {
        let mut sf = CtxSurface::new(&mut ctx);
        top_bar(&mut sf, state, indicator, bar_r);
        state.rows.build(&mut sf, &state.conversation, list_r.w, 1.0);

        if state.conversation.is_empty() {
            empty_state(&mut sf, list_r, state.can_ask());
        } else {
            // The tail, set before the list ticks its own physics. The
            // content is measured the way `view::list` measures it —
            // pitch times rows — so the two agree to the pixel.
            let content = virt::content_h(pitch, state.rows.rows().len());
            if state.conversation.follows_tail() {
                state.list.scroll.set_offset(content - list_r.h);
            }
            list::list(
                &mut sf,
                list_r,
                state.rows.rows(),
                &ListStyle::default(),
                Some(ListView {
                    state: &mut state.list,
                    hits: &mut state.hits,
                    id: 0,
                    // A conversation has nothing to select and no tree
                    // to open; what it needs from the view is the
                    // offset, the bar and the rectangles.
                    select: false,
                    scroll: true,
                    tree: false,
                    tooltip: false,
                }),
            );
            // Whether the reader is still at the bottom, decided from
            // where the list actually left the offset.
            let max = (content - list_r.h).max(0.0);
            state
                .conversation
                .set_follows_tail(state.list.scroll.offset() >= max - AT_BOTTOM_PX);
        }
    }

    // A press the field took, answered now that there is a glyph atlas
    // to measure with. Before the draw below, so the caret is already
    // where the hand put it in the very frame the click arrived in.
    if let Some((x, extend)) = state.field.take() {
        let at = text_input::hit(&mut ctx, field_r, &state.input, x);
        state.input.apply(text_input::InputMsg::Point { at, extend });
        state.caret_from = now;
    }

    let hint = placeholder(state, indicator);
    let style = InputStyle {
        placeholder: &hint,
        hover: field_r.contains(state.mouse.0, state.mouse.1),
        // Without a backend there is nothing to ask, and a field that
        // took a question it could not send would be lying about it.
        disabled: !state.can_ask(),
        focused_fallback: true,
    };
    text_input::draw(&mut ctx, field_r, &mut state.input, field_id(), &style).caret
}

/// The permanent indicator: which of the two providers is answering,
/// where from, and whether it is answering right now.
fn top_bar(sf: &mut impl Surface, state: &State, indicator: &Indicator, r: Rect) {
    let fill = sf.bed("component.topbar.fill");
    sf.rect(r, fill);

    let rule = sf.px("topbar.rule");
    if rule > 0.0 {
        let c = sf.color("component.topbar.rule");
        let y = r.bottom() - rule / 2.0;
        sf.line(r.x, y, r.right(), y, rule, c);
    }

    let pad_x = sf.px("topbar.pad_x");
    let pad_y = sf.px("topbar.pad_y");
    let gap = sf.px("topbar.cluster_gap");
    let inner = Rect::new(
        r.x + pad_x,
        r.y + pad_y,
        (r.w - 2.0 * pad_x).max(1.0),
        (r.h - 2.0 * pad_y).max(1.0),
    );

    // Which of the two, as a pill in the severity the choice carries:
    // `ok` while one of them can answer, `offline` when neither can.
    let severity = ui::sev_of(indicator.severity).unwrap_or_else(ui::sev_fallback);
    let pill = paint::badge(
        sf,
        inner,
        &indicator.label,
        Some(severity),
        BadgeStyle::FromTheme,
        Align::Left,
        1.0,
    );

    // The right cluster is about this turn rather than the session, and
    // is the only thing in the bar that moves while one is running. It is
    // measured BEFORE the left one is fitted, and that order is the whole
    // point: it is drawn whole or not at all — an ellipsised "ANSWERING ·
    // ESCAPE STOPS" would hide the one instruction that stops a runaway
    // answer — so the space it needs is taken off the top and the left
    // cluster is fitted into what is genuinely left. Fitting the left one
    // against the full width instead would run the credential's origin
    // straight under this word on any window narrow enough.
    let right = paint::bound_role(sf, "topbar.right.role", 1.0);
    let rink = sf.color("component.topbar.glyph");
    let word = turn_word(state.pending.is_some(), state.conversation.is_busy(), state.can_ask());
    let word_w = sf.measure(right.px, word, right.track);
    let ry = paint::center_line_y(sf, inner.y, inner.h, right.px, right.leading);
    sf.text(right.px, inner.right(), ry, word, rink, right.track, Align::Right);

    // What is answering and where from. The model the provider says it
    // used wins over the one that was asked for: they are not always the
    // same, and only the first is a fact about this conversation.
    let left = paint::bound_role(sf, "topbar.left.role", 1.0);
    let ink = sf.color("component.topbar.text");
    let model = state
        .conversation
        .answering()
        .unwrap_or(&indicator.model)
        .to_string();
    let said = if model.is_empty() {
        indicator.detail.clone()
    } else {
        format!("{model}  ·  {}", indicator.detail)
    };
    let x = inner.x + pill + gap;
    let room = (inner.right() - word_w - gap - x).max(1.0);
    let said = paint::fit_end(sf, left.px, &said, room, left.track);
    let y = paint::center_line_y(sf, inner.y, inner.h, left.px, left.leading);
    sf.text(left.px, x, y, &said, ink, left.track, Align::Left);
}

/// The right cluster's word: what the window is doing about THIS turn.
///
/// Four states, four different words, and no two of them read alike —
/// the one thing a person glancing at the bar has to be able to tell
/// apart is "it is working on it" from "it is waiting on me".
fn turn_word(pending: bool, busy: bool, can_ask: bool) -> &'static str {
    if pending {
        "WAITING FOR YOU"
    } else if busy {
        "ANSWERING · ESCAPE STOPS"
    } else if can_ask {
        "READY"
    } else {
        "NOTHING TO ASK"
    }
}

/// What an empty conversation says instead of nothing.
fn empty_state(sf: &mut impl Surface, r: Rect, can_ask: bool) {
    let role = paint::bound_role(sf, "emptystate.role", 1.0);
    // A percentage token bakes to a fraction of the box on its axis.
    let y = r.y + r.h * sf.px("emptystate.y_frac");
    let line = if can_ask {
        "Ask it something. Enter sends; Escape stops an answer that is arriving."
    } else {
        "Nothing can answer yet — the notices above say what is missing."
    };
    sf.text(role.px, r.cx(), y, line, role.color, role.track, Align::Center);
}

/// The field's placeholder — the shortest true sentence about what
/// typing here would do right now.
fn placeholder(state: &State, indicator: &Indicator) -> String {
    if state.pending.is_some() {
        "Enter allows the change above, Escape declines it".into()
    } else if !state.can_ask() {
        "no backend — nothing to ask".into()
    } else {
        format!("ask {}", indicator.label.to_lowercase())
    }
}

// ---------------------------------------------------------------------
// pacing
// ---------------------------------------------------------------------

/// How long until the next frame HAS to happen, or `None` when nothing
/// is moving and the loop may sleep until an event arrives.
///
/// Two answers folded together. The scrollbar's fade and a settle in
/// flight want ordinary frames, so they ask for one. The caret wants a
/// frame only when its phase turns over, and `motion.caret_blink` says
/// exactly when that is — so a window with a still conversation and a
/// blinking caret wakes twice a second, not sixty times.
fn next_wake(state: &State) -> Option<Duration> {
    let now = state.now();
    let mut soonest: Option<Duration> = None;
    let mut want = |d: Duration| {
        soonest = Some(match soonest {
            Some(cur) if cur <= d => cur,
            _ => d,
        });
    };

    let look = ScrollbarLook::from_theme();
    let fading = look.auto_hide
        && look.fade_ms > 0.0
        && ((now - state.list.scroll.moved_at()) * 1000.0) < look.fade_ms as f64;
    if state.list.scroll.velocity() != 0.0 || fading {
        want(FRAME);
    }
    if let Some(d) = caret_phase(now - state.caret_from) {
        want(d);
    }
    soonest
}

/// How long until the caret's blink turns over, given how long it has
/// been since the blink restarted.
///
/// `None` when the caret does not blink at all: reduced motion
/// (`motion.scale = 0`) freezes it fully visible, and a theme may turn
/// the effect off outright — in both cases there is nothing to wake for.
fn caret_phase(since: f64) -> Option<Duration> {
    static ENABLED: OnceLock<TokenId> = OnceLock::new();
    static SCALE: OnceLock<TokenId> = OnceLock::new();
    static PERIOD: OnceLock<TokenId> = OnceLock::new();
    static DUTY: OnceLock<TokenId> = OnceLock::new();

    let t = theme::resolved();
    let scale = t.px(tok(&SCALE, "motion.scale"));
    if scale <= 0.0 || !t.flag(tok(&ENABLED, "motion.caret_blink.enabled")) {
        return None;
    }
    let period = (t.px(tok(&PERIOD, "motion.caret_blink.period_ms")) * scale) as f64;
    if period <= 0.0 {
        return None;
    }
    let duty = f64::from(t.px(tok(&DUTY, "motion.caret_blink.duty")).clamp(0.0, 1.0));
    let on = period * duty;
    let phase = (since * 1000.0).max(0.0) % period;
    let until = if phase < on { on - phase } else { period - phase };
    Some(Duration::from_secs_f64(until.max(0.0) / 1000.0))
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caret_is_woken_at_its_own_phase_boundaries_and_not_on_a_frame_clock() {
        // The master's caret blinks: 1060 ms, on for 60 % of it.
        let start = caret_phase(0.0).expect("the master's caret blinks");
        assert!(start > FRAME * 10, "waking every frame to blink is the bug");
        let later = caret_phase(0.3).unwrap();
        assert!(later < start, "the wake moves with the phase");
        // Just past the turn into the off phase, what is left is the
        // rest of the period and not the whole of it.
        let off = caret_phase(0.65).unwrap();
        assert!(off < start);
    }

    #[test]
    fn the_bar_says_something_different_in_every_state_it_can_be_in() {
        let words = [
            turn_word(true, true, true),
            turn_word(false, true, true),
            turn_word(false, false, true),
            turn_word(false, false, false),
        ];
        for (i, a) in words.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &words[i + 1..] {
                assert_ne!(a, b, "two states of the window read the same");
            }
        }
        // A waiting change outranks an answer in flight: the worker is
        // blocked on the user, so "answering" would be a lie.
        assert_eq!(turn_word(true, true, true), turn_word(true, false, true));
        // Escape is the only way out of a running answer, so the bar says
        // so for as long as one is running.
        assert!(turn_word(false, true, true).contains("ESCAPE"));
    }

    /// A field drawn across the middle of a window, as the last frame
    /// would have left it.
    fn pointed_at_field() -> FieldPointer {
        let mut p = FieldPointer::new();
        p.r = Rect::new(100.0, 200.0, 300.0, 40.0);
        p
    }

    #[test]
    fn a_press_outside_the_field_is_left_for_the_conversation() {
        let mut p = pointed_at_field();
        assert!(!p.press(50.0, 210.0, false), "left of the field");
        assert!(!p.press(150.0, 50.0, false), "above it, in the column");
        assert_eq!(p.take(), None, "nothing was owed a caret");
    }

    #[test]
    fn a_press_in_the_field_places_the_caret_and_shift_extends_instead() {
        let mut p = pointed_at_field();
        assert!(p.press(150.0, 210.0, false));
        assert_eq!(p.take(), Some((150.0, false)));
        assert_eq!(p.take(), None, "a placement is owed exactly once");
        assert!(p.press(150.0, 210.0, true));
        assert_eq!(p.take(), Some((150.0, true)));
    }

    #[test]
    fn a_drag_that_leaves_the_field_is_still_the_same_drag() {
        let mut p = pointed_at_field();
        p.press(150.0, 210.0, false);
        let _ = p.take();
        // Out of the rectangle entirely — selecting the last word means
        // dragging past it, and the selection must not be dropped there.
        p.moved(9000.0);
        assert_eq!(p.take(), Some((9000.0, true)), "a drag always extends");
        p.release();
        p.moved(120.0);
        assert_eq!(p.take(), None, "a pointer nobody is holding selects nothing");
    }

    #[test]
    fn moving_over_the_field_without_pressing_it_moves_no_caret() {
        let mut p = pointed_at_field();
        p.moved(150.0);
        assert_eq!(p.take(), None);
    }

    /// Ids do not exist before the engine has been loaded, and it is
    /// loaded lazily — so a test that asks for one first has to load it,
    /// exactly as [`run`] does before its first frame.
    fn loaded() {
        let _ = theme::resolved();
    }

    #[test]
    fn the_master_declares_every_token_this_window_lays_itself_out_with() {
        loaded();
        for name in [
            "topbar.h",
            "topbar.pad_x",
            "topbar.pad_y",
            "topbar.cluster_gap",
            "topbar.rule",
            "topbar.left.role",
            "topbar.right.role",
            "panel.content_pad",
            "field.h",
            "space.6",
            "list.row_h",
            "list.gap",
            "emptystate.role",
            "emptystate.y_frac",
            "motion.scale",
            "motion.caret_blink.enabled",
            "motion.caret_blink.period_ms",
            "motion.caret_blink.duty",
            "surface.void",
            "component.topbar.fill",
            "component.topbar.text",
            "component.topbar.glyph",
            "component.topbar.rule",
        ] {
            assert!(
                theme::id(name).is_some(),
                "{name} is not declared by the master, so nothing may lay itself out with it"
            );
        }
    }

    #[test]
    fn the_page_is_laid_out_in_lengths_the_theme_gives_it() {
        let t = theme::resolved();
        let px = |name: &str| t.px(theme::id(name).unwrap());
        assert!(px("topbar.h") > 0.0);
        assert!(px("field.h") > 0.0);
        assert!(px("panel.content_pad") > 0.0);
        assert!(px("space.6") > 0.0);
        assert!(px("list.row_h") > 0.0);
        // A percentage bakes to a fraction, which is what the empty
        // state multiplies its box by.
        let frac = px("emptystate.y_frac");
        assert!((0.0..=1.0).contains(&frac), "y_frac baked as {frac}");
    }
}
