//! Terminal event stream
//!
//! This module provides async event streaming from the terminal using crossterm's
//! `EventStream`. It converts low-level crossterm events into application-specific
//! events, filtering out irrelevant events and normalizing key representations.
//!
//! # Event Filtering
//!
//! The event stream automatically filters:
//!
//! - Mouse events (except scroll wheel, which is forwarded)
//! - Key release and repeat events (only key press events are processed)
//! - Lock key events (`CapsLock`, `NumLock`, `ScrollLock`)
//! - Media and modifier-only key events
//!
//! # Async Design
//!
//! This module uses Tokio streams (`tokio_stream::StreamExt`) to provide async event
//! polling. The event stream can be efficiently integrated with Tokio's async runtime,
//! allowing the application to handle events concurrently with other async tasks.
//!
//! # Key Event Normalization
//!
//! Crossterm's `KeyEvent` includes raw key codes and modifiers. This module converts
//! them to string representations (e.g., "Enter", "Backspace", "F1") that are easier
//! to work with in the application layer and can be serialized for IPC communication.

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyEventState,
    KeyModifiers as CrosstermKeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::collections::VecDeque;
use std::io;
use std::time::Duration;
use tokio_stream::StreamExt;
use uncurses::event::{
    ColorScheme, Event as UncursesEvent, EventSource, Key as UncursesKey,
    KeyCode as UncursesKeyCode, KeyModifiers as UncursesKeyModifiers, Mouse as UncursesMouse,
    MouseButton as UncursesMouseButton,
};
use uncurses::terminal::TtyInput;

use crate::protocol::KeyModifiers;

/// Events emitted by the terminal.
///
/// This enum represents the subset of crossterm events that the application
/// cares about. Events are normalized to be easier to handle and serialize.
///
/// # Variants
///
/// - `Key`: A key press event with normalized key string and modifiers
/// - `Paste`: Bracketed paste content (multi-line clipboard paste)
/// - `Resize`: Terminal window size changed
/// - `FocusGained`/`FocusLost`: Focus change events (if terminal supports them)
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// Key press event.
    ///
    /// The `key` field contains a string representation of the key (e.g., "a", "Enter",
    /// "F1", "Up"). The `modifiers` field contains active modifier keys (Ctrl, Alt, Shift).
    Key {
        /// Key code as string representation
        key: String,
        /// Modifiers
        modifiers: KeyModifiers,
    },
    /// Paste event from bracketed paste mode.
    ///
    /// When the user pastes content (e.g., Ctrl+Shift+V in most terminals), the
    /// terminal sends the content as a paste event rather than individual key presses.
    /// This allows the application to distinguish typed text from pasted text.
    Paste(String),
    /// Terminal resized to new dimensions.
    ///
    /// Sent when the terminal window size changes (e.g., user resizes the window,
    /// or the terminal emulator's font size changes).
    Resize { width: u16, height: u16 },
    /// Terminal gained focus.
    ///
    /// Only sent if the terminal supports focus change events (enabled via crossterm's
    /// `EnableFocusChange` command).
    FocusGained,
    /// Terminal lost focus.
    ///
    /// Only sent if the terminal supports focus change events.
    FocusLost,
    /// Mouse scroll wheel event.
    ///
    /// Sent when the user scrolls the mouse wheel. Positive delta means scroll up,
    /// negative delta means scroll down.
    MouseScroll {
        /// Scroll direction: positive = up, negative = down
        delta: i8,
    },
}

/// Async stream of terminal events.
///
/// This wraps crossterm's `EventStream` and provides a simplified async interface
/// for polling terminal events. Events are automatically filtered and converted
/// to `TerminalEvent` instances.
///
/// # Example
///
/// ```no_run
/// use maestro_tui::terminal::TerminalEventStream;
///
/// # async fn example() {
/// let mut events = TerminalEventStream::new();
/// while let Some(event) = events.next().await {
///     match event {
///         maestro_tui::terminal::TerminalEvent::Key { key, .. } => {
///             println!("Key pressed: {}", key);
///         }
///         _ => {}
///     }
/// }
/// # }
/// ```
pub struct TerminalEventStream {
    inner: EventStream,
}

impl TerminalEventStream {
    /// Create a new terminal event stream
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: EventStream::new(),
        }
    }

    /// Get the next terminal event
    pub async fn next(&mut self) -> Option<TerminalEvent> {
        loop {
            match self.inner.next().await {
                Some(Ok(event)) => {
                    if let Some(te) = convert_event(event) {
                        return Some(te);
                    }
                    // Event was filtered out, continue
                }
                Some(Err(_)) => continue,
                None => return None,
            }
        }
    }
}

impl Default for TerminalEventStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Rich terminal events used by the interactive app.
///
/// Unlike crossterm 0.28, uncurses decodes terminal query replies as typed
/// events. That lets the app query color-scheme capabilities without leaking
/// reply bytes into the composer.
#[derive(Debug)]
pub(crate) enum AppTerminalEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize { width: u16, height: u16 },
    FocusGained,
    FocusLost,
    BackgroundColor { red: u8, green: u8, blue: u8 },
    ColorScheme(ColorScheme),
    ThemeReportingStatus(uncurses::ansi::mode::ModeSetting),
}

impl AppTerminalEvent {
    /// Preserve the legacy crossterm path as a fallback when the controlling
    /// terminal cannot be opened by uncurses.
    pub(crate) fn from_crossterm(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) => Some(Self::Key(key)),
            Event::Mouse(mouse) => Some(Self::Mouse(mouse)),
            Event::Paste(text) => Some(Self::Paste(text)),
            Event::Resize(width, height) => Some(Self::Resize { width, height }),
            Event::FocusGained => Some(Self::FocusGained),
            Event::FocusLost => Some(Self::FocusLost),
        }
    }
}

/// Synchronous uncurses event reader for the main render loop.
pub(crate) struct TerminalEventReader {
    source: EventSource<TtyInput>,
    state: EventConversionState,
}

#[derive(Default)]
struct EventConversionState {
    pending: VecDeque<UncursesEvent>,
    paste: Option<Vec<u8>>,
}

impl TerminalEventReader {
    /// Open the controlling terminal and create a protocol-aware event source.
    pub(crate) fn open() -> io::Result<Self> {
        let terminal = uncurses::terminal::Terminal::open()?;
        Ok(Self {
            source: EventSource::new(terminal.input())?,
            state: EventConversionState::default(),
        })
    }

    /// Cloneable handle that interrupts a blocking [`Self::poll`].
    pub(crate) fn waker(&self) -> uncurses::event::Waker {
        self.source.waker()
    }

    /// Poll for one application event.
    ///
    /// Query replies unsupported by the application are deliberately consumed
    /// here rather than reinterpreted as user input.
    pub(crate) fn poll(&mut self, timeout: Duration) -> io::Result<Option<AppTerminalEvent>> {
        if self.state.pending.is_empty() && !self.source.poll(Some(timeout))? {
            return Ok(None);
        }

        while let Some(event) = self
            .state
            .pending
            .pop_front()
            .or_else(|| self.source.try_read())
        {
            if let Some(event) = self.state.convert_event(event) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

impl EventConversionState {
    fn convert_event(&mut self, event: UncursesEvent) -> Option<AppTerminalEvent> {
        match event {
            UncursesEvent::KeyPress(key) => {
                convert_uncurses_key(key, KeyEventKind::Press).map(AppTerminalEvent::Key)
            }
            UncursesEvent::KeyRepeat(key) => {
                convert_uncurses_key(key, KeyEventKind::Repeat).map(AppTerminalEvent::Key)
            }
            UncursesEvent::KeyRelease(_) => None,
            UncursesEvent::MouseClick(mouse) => {
                convert_uncurses_mouse(mouse, false).map(AppTerminalEvent::Mouse)
            }
            UncursesEvent::MouseRelease(mouse) => {
                convert_uncurses_mouse(mouse, true).map(AppTerminalEvent::Mouse)
            }
            UncursesEvent::MouseWheel(mouse) => {
                convert_uncurses_mouse(mouse, false).map(AppTerminalEvent::Mouse)
            }
            UncursesEvent::MouseMove(_) => None,
            UncursesEvent::Resize(size) => Some(AppTerminalEvent::Resize {
                width: size.col,
                height: size.row,
            }),
            UncursesEvent::FocusIn => Some(AppTerminalEvent::FocusGained),
            UncursesEvent::FocusOut => Some(AppTerminalEvent::FocusLost),
            UncursesEvent::PasteStart => {
                self.paste = Some(Vec::new());
                None
            }
            UncursesEvent::PasteChunk(chunk) => {
                self.paste.get_or_insert_with(Vec::new).extend(chunk);
                None
            }
            UncursesEvent::PasteEnd => self
                .paste
                .take()
                .map(|bytes| AppTerminalEvent::Paste(String::from_utf8_lossy(&bytes).into_owned())),
            UncursesEvent::BackgroundColor(color) => {
                let (red, green, blue) = color.to_rgb();
                Some(AppTerminalEvent::BackgroundColor { red, green, blue })
            }
            UncursesEvent::ColorScheme(scheme) => Some(AppTerminalEvent::ColorScheme(scheme)),
            UncursesEvent::ModeReport { mode, setting }
                if mode == uncurses::ansi::mode::Mode::LIGHT_DARK =>
            {
                Some(AppTerminalEvent::ThemeReportingStatus(setting))
            }
            UncursesEvent::Multi(events) => {
                self.pending.extend(events);
                None
            }
            _ => None,
        }
    }
}

fn convert_uncurses_key(key: UncursesKey, kind: KeyEventKind) -> Option<KeyEvent> {
    let code = if key.code == UncursesKeyCode::Tab
        && key.modifiers.contains(UncursesKeyModifiers::SHIFT)
    {
        KeyCode::BackTab
    } else if matches!(key.code, UncursesKeyCode::Char(_)) {
        match key.text.as_deref() {
            Some(text) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(character), None) if !character.is_control() => KeyCode::Char(character),
                    _ => convert_uncurses_key_code(key.code)?,
                }
            }
            None => convert_uncurses_key_code(key.code)?,
        }
    } else {
        convert_uncurses_key_code(key.code)?
    };
    Some(KeyEvent {
        code,
        modifiers: convert_uncurses_modifiers(key.modifiers),
        kind,
        state: KeyEventState::NONE,
    })
}

fn convert_uncurses_key_code(code: UncursesKeyCode) -> Option<KeyCode> {
    Some(match code {
        UncursesKeyCode::Char(character) => KeyCode::Char(character),
        UncursesKeyCode::F(number) => KeyCode::F(number),
        UncursesKeyCode::Up | UncursesKeyCode::KpUp => KeyCode::Up,
        UncursesKeyCode::Down | UncursesKeyCode::KpDown => KeyCode::Down,
        UncursesKeyCode::Left | UncursesKeyCode::KpLeft => KeyCode::Left,
        UncursesKeyCode::Right | UncursesKeyCode::KpRight => KeyCode::Right,
        UncursesKeyCode::Home | UncursesKeyCode::KpHome => KeyCode::Home,
        UncursesKeyCode::End | UncursesKeyCode::KpEnd => KeyCode::End,
        UncursesKeyCode::PageUp | UncursesKeyCode::KpPageUp => KeyCode::PageUp,
        UncursesKeyCode::PageDown | UncursesKeyCode::KpPageDown => KeyCode::PageDown,
        UncursesKeyCode::Backspace => KeyCode::Backspace,
        UncursesKeyCode::Delete | UncursesKeyCode::KpDelete => KeyCode::Delete,
        UncursesKeyCode::Insert | UncursesKeyCode::KpInsert => KeyCode::Insert,
        UncursesKeyCode::Tab => KeyCode::Tab,
        UncursesKeyCode::Enter | UncursesKeyCode::KpEnter => KeyCode::Enter,
        UncursesKeyCode::Space => KeyCode::Char(' '),
        UncursesKeyCode::Escape => KeyCode::Esc,
        UncursesKeyCode::KpAdd => KeyCode::Char('+'),
        UncursesKeyCode::KpSubtract => KeyCode::Char('-'),
        UncursesKeyCode::KpMultiply => KeyCode::Char('*'),
        UncursesKeyCode::KpDivide => KeyCode::Char('/'),
        UncursesKeyCode::KpDecimal => KeyCode::Char('.'),
        UncursesKeyCode::KpEqual => KeyCode::Char('='),
        UncursesKeyCode::KpSeparator => KeyCode::Char(','),
        UncursesKeyCode::Kp0 => KeyCode::Char('0'),
        UncursesKeyCode::Kp1 => KeyCode::Char('1'),
        UncursesKeyCode::Kp2 => KeyCode::Char('2'),
        UncursesKeyCode::Kp3 => KeyCode::Char('3'),
        UncursesKeyCode::Kp4 => KeyCode::Char('4'),
        UncursesKeyCode::Kp5 => KeyCode::Char('5'),
        UncursesKeyCode::Kp6 => KeyCode::Char('6'),
        UncursesKeyCode::Kp7 => KeyCode::Char('7'),
        UncursesKeyCode::Kp8 => KeyCode::Char('8'),
        UncursesKeyCode::Kp9 => KeyCode::Char('9'),
        _ => return None,
    })
}

fn convert_uncurses_modifiers(modifiers: UncursesKeyModifiers) -> CrosstermKeyModifiers {
    let mut converted = CrosstermKeyModifiers::empty();
    for (source, target) in [
        (UncursesKeyModifiers::SHIFT, CrosstermKeyModifiers::SHIFT),
        (UncursesKeyModifiers::ALT, CrosstermKeyModifiers::ALT),
        (UncursesKeyModifiers::CTRL, CrosstermKeyModifiers::CONTROL),
        (UncursesKeyModifiers::META, CrosstermKeyModifiers::META),
        (UncursesKeyModifiers::SUPER, CrosstermKeyModifiers::SUPER),
        (UncursesKeyModifiers::HYPER, CrosstermKeyModifiers::HYPER),
    ] {
        if modifiers.contains(source) {
            converted.insert(target);
        }
    }
    converted
}

fn convert_uncurses_mouse(mouse: UncursesMouse, release: bool) -> Option<MouseEvent> {
    let kind = match mouse.button {
        UncursesMouseButton::WheelUp => MouseEventKind::ScrollUp,
        UncursesMouseButton::WheelDown => MouseEventKind::ScrollDown,
        UncursesMouseButton::WheelLeft => MouseEventKind::ScrollLeft,
        UncursesMouseButton::WheelRight => MouseEventKind::ScrollRight,
        UncursesMouseButton::Left => button_event(MouseButton::Left, release),
        UncursesMouseButton::Middle => button_event(MouseButton::Middle, release),
        UncursesMouseButton::Right => button_event(MouseButton::Right, release),
        UncursesMouseButton::None => MouseEventKind::Moved,
        _ => return None,
    };
    Some(MouseEvent {
        kind,
        column: mouse.x,
        row: mouse.y,
        modifiers: convert_uncurses_modifiers(mouse.modifiers),
    })
}

fn button_event(button: MouseButton, release: bool) -> MouseEventKind {
    if release {
        MouseEventKind::Up(button)
    } else {
        MouseEventKind::Down(button)
    }
}

/// Convert crossterm event to our normalized event type.
///
/// This function maps crossterm's raw events to our simplified `TerminalEvent` enum,
/// filtering out events we don't care about (e.g., mouse events, key releases).
///
/// Returns `None` for filtered events, which causes the stream to skip them and
/// continue polling for the next event.
fn convert_event(event: Event) -> Option<TerminalEvent> {
    match event {
        Event::Key(key_event) => convert_key_event(key_event),
        Event::Paste(text) => Some(TerminalEvent::Paste(text)),
        Event::Resize(width, height) => Some(TerminalEvent::Resize { width, height }),
        Event::FocusGained => Some(TerminalEvent::FocusGained),
        Event::FocusLost => Some(TerminalEvent::FocusLost),
        Event::Mouse(mouse) => convert_mouse_event(mouse),
    }
}

/// Convert crossterm mouse event to our format.
///
/// Only scroll wheel events are converted; other mouse events are ignored.
fn convert_mouse_event(mouse: MouseEvent) -> Option<TerminalEvent> {
    match mouse.kind {
        MouseEventKind::ScrollUp => Some(TerminalEvent::MouseScroll { delta: 1 }),
        MouseEventKind::ScrollDown => Some(TerminalEvent::MouseScroll { delta: -1 }),
        _ => None, // Ignore other mouse events (clicks, movement, etc.)
    }
}

/// Convert crossterm key event to our format.
///
/// Filters out key release and repeat events, keeping only key press events.
/// This is the most common behavior for TUI applications - we only care about
/// when a key is first pressed, not when it's released or auto-repeated.
///
/// Returns `None` for filtered events or keys we don't recognize.
fn convert_key_event(key: KeyEvent) -> Option<TerminalEvent> {
    // Only handle key press events, not release or repeat
    if key.kind != KeyEventKind::Press {
        return None;
    }

    let key_str = key_code_to_string(key.code)?;
    let modifiers = key.modifiers.into();

    Some(TerminalEvent::Key {
        key: key_str,
        modifiers,
    })
}

/// Convert crossterm key code to string representation.
///
/// This normalizes key codes to predictable string values that can be used for
/// key binding matching and IPC serialization. Character keys are converted to
/// their string form (e.g., 'a' -> "a"), while special keys use consistent names
/// (e.g., `KeyCode::Enter` -> "Enter", `KeyCode::F(1)` -> "F1").
///
/// Returns `None` for keys we don't want to handle (null, lock keys, media keys, etc.).
fn key_code_to_string(code: KeyCode) -> Option<String> {
    let s = match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Esc => "Escape".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Null => return None,
        KeyCode::CapsLock => return None,
        KeyCode::ScrollLock => return None,
        KeyCode::NumLock => return None,
        KeyCode::PrintScreen => return None,
        KeyCode::Pause => return None,
        KeyCode::Menu => return None,
        KeyCode::KeypadBegin => return None,
        KeyCode::Media(_) => return None,
        KeyCode::Modifier(_) => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uncurses::event::{
        Key as UncursesKey, KeyCode as UncursesKeyCode, KeyModifiers as UncursesKeyModifiers,
    };

    #[test]
    fn test_key_code_to_string() {
        assert_eq!(
            key_code_to_string(KeyCode::Char('a')),
            Some("a".to_string())
        );
        assert_eq!(
            key_code_to_string(KeyCode::Enter),
            Some("Enter".to_string())
        );
        assert_eq!(key_code_to_string(KeyCode::F(1)), Some("F1".to_string()));
        assert_eq!(key_code_to_string(KeyCode::Null), None);
    }

    #[test]
    fn uncurses_shifted_text_preserves_the_typed_character() {
        let mut key = UncursesKey::new(UncursesKeyCode::Char('/'), UncursesKeyModifiers::SHIFT);
        key.text = Some("?".to_string());

        let converted = convert_uncurses_key(key, crossterm::event::KeyEventKind::Press)
            .expect("printable key should convert");

        assert_eq!(converted.code, KeyCode::Char('?'));
        assert!(
            converted
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn uncurses_named_keys_ignore_protocol_text() {
        let mut key = UncursesKey::new(UncursesKeyCode::Enter, UncursesKeyModifiers::empty());
        key.text = Some("\r".to_string());

        let converted = convert_uncurses_key(key, crossterm::event::KeyEventKind::Press)
            .expect("named key should convert");

        assert_eq!(converted.code, KeyCode::Enter);
    }

    #[test]
    fn uncurses_control_characters_keep_their_logical_key() {
        let mut key = UncursesKey::new(UncursesKeyCode::Char('c'), UncursesKeyModifiers::CTRL);
        key.text = Some("\u{3}".to_string());

        let converted = convert_uncurses_key(key, crossterm::event::KeyEventKind::Press)
            .expect("control key should convert");

        assert_eq!(converted.code, KeyCode::Char('c'));
        assert!(
            converted
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn uncurses_modifier_mapping_keeps_modern_modifier_bits() {
        let key = UncursesKey::new(
            UncursesKeyCode::Char('x'),
            UncursesKeyModifiers::CTRL
                | UncursesKeyModifiers::ALT
                | UncursesKeyModifiers::META
                | UncursesKeyModifiers::SUPER
                | UncursesKeyModifiers::HYPER,
        );

        let converted = convert_uncurses_key(key, crossterm::event::KeyEventKind::Press)
            .expect("character key should convert");

        let modifiers = converted.modifiers;
        assert!(modifiers.contains(crossterm::event::KeyModifiers::CONTROL));
        assert!(modifiers.contains(crossterm::event::KeyModifiers::ALT));
        assert!(modifiers.contains(crossterm::event::KeyModifiers::META));
        assert!(modifiers.contains(crossterm::event::KeyModifiers::SUPER));
        assert!(modifiers.contains(crossterm::event::KeyModifiers::HYPER));
    }

    #[test]
    fn uncurses_paste_chunks_are_reassembled_lossily() {
        let mut state = EventConversionState::default();
        assert!(state.convert_event(UncursesEvent::PasteStart).is_none());
        assert!(
            state
                .convert_event(UncursesEvent::PasteChunk(vec![b'a', 0xff]))
                .is_none()
        );

        let event = state
            .convert_event(UncursesEvent::PasteEnd)
            .expect("paste end should emit the assembled paste");
        assert!(matches!(event, AppTerminalEvent::Paste(text) if text == "a\u{fffd}"));
    }

    #[test]
    fn uncurses_multi_events_preserve_fifo_order() {
        let mut state = EventConversionState::default();
        assert!(
            state
                .convert_event(UncursesEvent::Multi(vec![
                    UncursesEvent::FocusIn,
                    UncursesEvent::Resize(uncurses::terminal::Winsize {
                        row: 42,
                        col: 120,
                        xpixel: 0,
                        ypixel: 0,
                    }),
                ]))
                .is_none()
        );

        let first = state.pending.pop_front().expect("first nested event");
        let second = state.pending.pop_front().expect("second nested event");
        assert!(matches!(
            state.convert_event(first),
            Some(AppTerminalEvent::FocusGained)
        ));
        assert!(matches!(
            state.convert_event(second),
            Some(AppTerminalEvent::Resize {
                width: 120,
                height: 42
            })
        ));
    }

    #[test]
    fn uncurses_theme_replies_stay_typed() {
        let mut state = EventConversionState::default();
        assert!(matches!(
            state.convert_event(UncursesEvent::BackgroundColor(uncurses::color::Color::Rgb(
                1, 2, 3
            ))),
            Some(AppTerminalEvent::BackgroundColor {
                red: 1,
                green: 2,
                blue: 3
            })
        ));
        assert!(matches!(
            state.convert_event(UncursesEvent::ColorScheme(ColorScheme::Light)),
            Some(AppTerminalEvent::ColorScheme(ColorScheme::Light))
        ));
        assert!(matches!(
            state.convert_event(UncursesEvent::ModeReport {
                mode: uncurses::ansi::mode::Mode::LIGHT_DARK,
                setting: uncurses::ansi::mode::ModeSetting::Set,
            }),
            Some(AppTerminalEvent::ThemeReportingStatus(
                uncurses::ansi::mode::ModeSetting::Set
            ))
        ));
    }

    #[test]
    fn crossterm_fallback_keeps_resize_and_focus_events() {
        assert!(matches!(
            AppTerminalEvent::from_crossterm(Event::Resize(120, 42)),
            Some(AppTerminalEvent::Resize {
                width: 120,
                height: 42
            })
        ));
        assert!(matches!(
            AppTerminalEvent::from_crossterm(Event::FocusLost),
            Some(AppTerminalEvent::FocusLost)
        ));
    }
}
