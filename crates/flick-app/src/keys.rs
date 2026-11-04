//! Every hotkey in one table: which key runs which command. The key handler
//! (`Viewer::on_key`) looks keys up here, and the menu shows each command's
//! keys from here, so the two can't disagree. Changing a key is one line in
//! `BINDINGS`.

use winit::event::KeyEvent;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

use crate::view::{Filter, Fit};

/// What a key (or a menu item) does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Next,
    Previous,
    RandomNext,
    RandomBack,
    First,
    Last,
    NextFolder,
    PreviousFolder,
    Back,
    Forward,
    NextTab,
    PreviousTab,
    /// The tab at this place on the strip (0: the first).
    Tab(u8),
    DuplicateTab,
    DuplicateInNewWindow,
    CloseTab,
    ReopenClosed,
    TabLock,
    AddStar,
    RemoveStar,
    Copy,
    ShowInExplorer,
    Delete,
    DeletePermanently,
    Fit(Fit),
    StretchSmall,
    Filter(Filter),
    ZoomIn,
    ZoomOut,
    Info,
    /// Rotate ▸ this many quarter turns clockwise.
    Turn(u8),
    FlipH,
    FlipV,
    NextRotation,
    NextFlip,
    Slideshow,
    SlideShorter,
    SlideLonger,
    /// Full screen on or off (F11, Enter).
    FullScreen,
    /// Esc: out of full screen (nothing otherwise).
    LeaveFullScreen,
    Menu,
    DebugInfo,
    /// Debugging: the next frame finds the graphics device lost.
    SimulateLoss,
}

impl Command {
    /// Whether holding its key runs it again on each repeat (else once per
    /// press).
    pub fn repeats(self) -> bool {
        matches!(
            self,
            C::Next | C::Previous | C::RandomNext | C::RandomBack | C::Back | C::Forward | C::NextTab | C::PreviousTab | C::CloseTab | C::ZoomIn | C::ZoomOut | C::SlideShorter | C::SlideLonger
        )
    }

    /// Paging, which a held key does only as fast as pictures get on screen.
    pub fn pages(self) -> bool {
        matches!(self, C::Next | C::Previous | C::RandomNext | C::RandomBack)
    }

    /// Moving by hand through pictures or tabs, which stops a slideshow.
    pub fn navigates(self) -> bool {
        self.pages() || matches!(self, C::First | C::Last | C::NextFolder | C::PreviousFolder | C::Back | C::Forward | C::NextTab | C::PreviousTab | C::Tab(_))
    }
}

/// A key, the way a binding names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    /// By its place on the keyboard, so letters and digits work in any
    /// layout (as Honeyview's do): Z is Z on a Russian keyboard too.
    Code(KeyCode),
    /// A named key: arrows, Tab, Enter, F12…
    Named(NamedKey),
    /// A character, whichever key types it. Shift doesn't count: "+" takes
    /// it on most layouts.
    Char(&'static str),
}

/// A key with its modifiers, which must match exactly (Ctrl+Shift+C is
/// not Ctrl+C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub on: On,
}

const fn plain(on: On) -> Chord {
    Chord { ctrl: false, shift: false, alt: false, on }
}

const fn ctrl(on: On) -> Chord {
    Chord { ctrl: true, ..plain(on) }
}

const fn shift(on: On) -> Chord {
    Chord { shift: true, ..plain(on) }
}

const fn alt(on: On) -> Chord {
    Chord { alt: true, ..plain(on) }
}

const fn ctrl_shift(on: On) -> Chord {
    Chord { ctrl: true, shift: true, ..plain(on) }
}

use Command as C;
use KeyCode as K;
use NamedKey as N;
use On::{Char, Code, Named};

/// Every hotkey. A command's keys show in the menu in this order.
pub const BINDINGS: &[(Chord, Command)] = &[
    // Navigate (with Tab Lock on, the paging keys switch tabs instead).
    (plain(Named(N::ArrowRight)), C::Next),
    (plain(Named(N::Space)), C::Next),
    (plain(Named(N::PageDown)), C::Next),
    (plain(Code(K::KeyD)), C::Next),
    (plain(Named(N::ArrowLeft)), C::Previous),
    (plain(Named(N::Backspace)), C::Previous),
    (plain(Named(N::PageUp)), C::Previous),
    (plain(Code(K::KeyA)), C::Previous),
    (plain(Named(N::ArrowDown)), C::RandomNext),
    (plain(Code(K::KeyS)), C::RandomNext),
    (plain(Named(N::ArrowUp)), C::RandomBack),
    (plain(Code(K::KeyW)), C::RandomBack),
    (plain(Named(N::Home)), C::First),
    (plain(Named(N::End)), C::Last),
    (plain(Code(K::BracketRight)), C::NextFolder),
    (plain(Code(K::BracketLeft)), C::PreviousFolder),
    // The keyboard's Back and Forward keys (the mouse's side buttons too).
    (plain(Named(N::BrowserBack)), C::Back),
    (plain(Named(N::BrowserForward)), C::Forward),
    // Tabs.
    (ctrl(Named(N::Tab)), C::NextTab),
    (ctrl_shift(Named(N::Tab)), C::PreviousTab),
    (ctrl(Code(K::Digit1)), C::Tab(0)),
    (ctrl(Code(K::Digit2)), C::Tab(1)),
    (ctrl(Code(K::Digit3)), C::Tab(2)),
    (ctrl(Code(K::Digit4)), C::Tab(3)),
    (ctrl(Code(K::Digit5)), C::Tab(4)),
    (ctrl(Code(K::Digit6)), C::Tab(5)),
    (ctrl(Code(K::Digit7)), C::Tab(6)),
    (ctrl(Code(K::Digit8)), C::Tab(7)),
    (ctrl(Code(K::Digit9)), C::Tab(8)),
    (ctrl(Code(K::KeyT)), C::DuplicateTab),
    (ctrl(Code(K::KeyN)), C::DuplicateInNewWindow),
    (ctrl(Code(K::KeyW)), C::CloseTab),
    (ctrl_shift(Code(K::KeyT)), C::ReopenClosed),
    (plain(Code(K::KeyL)), C::TabLock),
    // The picture's file.
    (ctrl(Code(K::KeyF)), C::AddStar),
    (shift(Code(K::KeyF)), C::RemoveStar),
    (ctrl(Code(K::KeyC)), C::Copy),
    (ctrl(Named(N::Enter)), C::ShowInExplorer),
    (plain(Named(N::Delete)), C::Delete),
    (shift(Named(N::Delete)), C::DeletePermanently),
    // View: Honeyview's keys (its Soften+Sharpen S is random paging here).
    (plain(Code(K::Digit9)), C::Fit(Fit::Window)),
    (plain(Code(K::Digit6)), C::Fit(Fit::Fill)),
    (plain(Code(K::Digit8)), C::Fit(Fit::Width)),
    (plain(Code(K::Digit7)), C::Fit(Fit::Height)),
    (plain(Code(K::Digit0)), C::Fit(Fit::Original)),
    (plain(Code(K::KeyZ)), C::StretchSmall),
    (plain(Code(K::KeyU)), C::Filter(Filter::Nearest)),
    (plain(Code(K::KeyI)), C::Filter(Filter::Soften)),
    (plain(Char("+")), C::ZoomIn),
    (plain(Char("=")), C::ZoomIn),
    (plain(Char("-")), C::ZoomOut),
    (plain(Named(N::Tab)), C::Info),
    // Rotate.
    (alt(Named(N::ArrowUp)), C::Turn(0)),
    (alt(Named(N::ArrowRight)), C::Turn(1)),
    (alt(Named(N::ArrowDown)), C::Turn(2)),
    (alt(Named(N::ArrowLeft)), C::Turn(3)),
    (ctrl(Code(K::KeyR)), C::NextRotation),
    (alt(Code(K::KeyH)), C::FlipH),
    (alt(Code(K::KeyV)), C::FlipV),
    (alt(Code(K::KeyF)), C::NextFlip),
    // Slideshow.
    (ctrl(Code(K::KeyG)), C::Slideshow),
    (ctrl(Code(K::BracketLeft)), C::SlideShorter),
    (ctrl(Code(K::BracketRight)), C::SlideLonger),
    // The rest.
    (plain(Named(N::F11)), C::FullScreen),
    (plain(Named(N::Enter)), C::FullScreen),
    (plain(Named(N::Escape)), C::LeaveFullScreen),
    (plain(Named(N::ContextMenu)), C::Menu),
    (plain(Named(N::F12)), C::DebugInfo),
    (shift(Named(N::F12)), C::SimulateLoss),
];

impl Chord {
    fn matches(&self, physical: PhysicalKey, logical: &Key<&str>, modifiers: ModifiersState) -> bool {
        let shift_counts = !matches!(self.on, Char(_));
        if self.ctrl != modifiers.control_key() || self.alt != modifiers.alt_key() || (shift_counts && self.shift != modifiers.shift_key()) {
            return false;
        }
        match self.on {
            Code(code) => physical == PhysicalKey::Code(code),
            Named(named) => *logical == Key::Named(named),
            Char(c) => *logical == Key::Character(c),
        }
    }

    /// As the menu shows it: "Ctrl+Shift+T", "→".
    fn text(&self) -> String {
        let mut text = String::new();
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                text.push_str(name);
            }
        }
        text.push_str(&key_name(self.on));
        text
    }
}

fn key_name(on: On) -> String {
    match on {
        Code(K::BracketLeft) => "[".into(),
        Code(K::BracketRight) => "]".into(),
        Code(code) => {
            let name = format!("{code:?}");
            name.strip_prefix("Key").or_else(|| name.strip_prefix("Digit")).unwrap_or(&name).to_string()
        }
        Named(named) => match named {
            N::ArrowRight => "→",
            N::ArrowLeft => "←",
            N::ArrowUp => "↑",
            N::ArrowDown => "↓",
            N::PageDown => "PgDn",
            N::PageUp => "PgUp",
            N::Delete => "Del",
            N::Escape => "Esc",
            N::ContextMenu => "Menu",
            N::BrowserBack => "Browser Back",
            N::BrowserForward => "Browser Forward",
            named => return format!("{named:?}"),
        }
        .into(),
        Char(c) => c.into(),
    }
}

/// The command a key press runs, if any.
pub fn command(event: &KeyEvent, modifiers: ModifiersState) -> Option<Command> {
    lookup(event.physical_key, event.logical_key.as_ref(), modifiers)
}

fn lookup(physical: PhysicalKey, logical: Key<&str>, modifiers: ModifiersState) -> Option<Command> {
    BINDINGS.iter().find(|(chord, _)| chord.matches(physical, &logical, modifiers)).map(|&(_, command)| command)
}

/// A command's keys, as the menu shows them: "→, Space, PgDn, D" ("" if none).
pub fn keys(command: Command) -> String {
    BINDINGS.iter().filter(|(_, c)| *c == command).map(|(chord, _)| chord.text()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(physical: KeyCode, logical: Key<&str>, modifiers: ModifiersState) -> Option<Command> {
        lookup(PhysicalKey::Code(physical), logical, modifiers)
    }

    #[test]
    fn no_two_bindings_share_a_key() {
        for (i, (a, _)) in BINDINGS.iter().enumerate() {
            for (b, _) in &BINDINGS[i + 1..] {
                let same_shift = a.shift == b.shift || matches!(a.on, Char(_));
                assert!(!(a.on == b.on && a.ctrl == b.ctrl && a.alt == b.alt && same_shift), "{} is bound twice", a.text());
            }
        }
    }

    #[test]
    fn keys_find_their_commands() {
        let none = ModifiersState::empty();
        let (ctrl, shift) = (ModifiersState::CONTROL, ModifiersState::SHIFT);
        assert_eq!(press(K::KeyT, Key::Character("t"), ctrl), Some(C::DuplicateTab));
        assert_eq!(press(K::KeyT, Key::Character("T"), ctrl | shift), Some(C::ReopenClosed));
        // Modifiers match exactly.
        assert_eq!(press(K::KeyC, Key::Character("C"), ctrl | shift), None);
        assert_eq!(press(K::KeyF, Key::Character("f"), none), None);
        // Letters by place: Z on a Russian layout types "я".
        assert_eq!(press(K::KeyZ, Key::Character("я"), none), Some(C::StretchSmall));
        // Tab alone is the info panel, with Ctrl the next tab.
        assert_eq!(press(K::Tab, Key::Named(N::Tab), none), Some(C::Info));
        assert_eq!(press(K::Tab, Key::Named(N::Tab), ctrl), Some(C::NextTab));
        // "+" with or without Shift (layouts differ), but not with Ctrl.
        assert_eq!(press(K::Equal, Key::Character("+"), shift), Some(C::ZoomIn));
        assert_eq!(press(K::NumpadAdd, Key::Character("+"), none), Some(C::ZoomIn));
        assert_eq!(press(K::Equal, Key::Character("+"), ctrl), None);
        assert_eq!(press(K::ArrowLeft, Key::Named(N::ArrowLeft), ModifiersState::ALT), Some(C::Turn(3)));
    }

    #[test]
    fn the_menu_shows_each_commands_keys() {
        assert_eq!(keys(C::Next), "→, Space, PgDn, D");
        assert_eq!(keys(C::ReopenClosed), "Ctrl+Shift+T");
        assert_eq!(keys(C::Tab(8)), "Ctrl+9");
        assert_eq!(keys(C::SlideShorter), "Ctrl+[");
        assert_eq!(keys(C::Fit(Fit::Original)), "0");
        assert_eq!(keys(C::Info), "Tab");
        assert_eq!(keys(C::Turn(0)), "Alt+↑");
        assert_eq!(keys(C::FullScreen), "F11, Enter");
    }
}
