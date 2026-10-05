//! The system-wide "go live with the window in front" shortcut.
//!
//! Shortcuts are recorded from the keys physically held down (Windows
//! virtual-key codes) rather than from the typed character: on layouts like
//! ABNT2, Ctrl+Alt+letter is AltGr and types symbols instead.

use std::fmt;

/// A key combination, stored in settings as text like "Ctrl+Alt+S".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Windows virtual-key code.
    pub key: u16,
}

/// Keys a shortcut can end in, with the names shown and saved. Layout-
/// dependent punctuation keys are left out: their names would be wrong on
/// some keyboards.
fn key_names() -> Vec<(u16, String)> {
    let mut keys: Vec<(u16, String)> = Vec::new();
    keys.extend((b'A'..=b'Z').map(|c| (c as u16, (c as char).to_string())));
    keys.extend((b'0'..=b'9').map(|c| (c as u16, (c as char).to_string())));
    keys.extend((1..=24).map(|n| (0x6f + n, format!("F{n}"))));
    keys.extend((0..=9).map(|n| (0x60 + n, format!("Num{n}"))));
    for (code, name) in [
        (0x20, "Space"),
        (0x2d, "Insert"),
        (0x2e, "Delete"),
        (0x24, "Home"),
        (0x23, "End"),
        (0x21, "PageUp"),
        (0x22, "PageDown"),
        (0x25, "Left"),
        (0x26, "Up"),
        (0x27, "Right"),
        (0x28, "Down"),
        (0x13, "Pause"),
        // What Pause becomes with Ctrl held.
        (0x03, "Break"),
        (0x91, "ScrollLock"),
    ] {
        keys.push((code, name.to_owned()));
    }
    keys
}

/// Keys that never type anything, so they're safe as shortcuts on their own
/// (or with just Shift): F1-F24, Pause/Break and Scroll Lock.
fn is_non_typing_key(key: u16) -> bool {
    (0x70..=0x87).contains(&key) || matches!(key, 0x13 | 0x03 | 0x91)
}

impl Shortcut {
    pub fn parse(text: &str) -> Option<Self> {
        let mut shortcut = Shortcut { ctrl: false, alt: false, shift: false, win: false, key: 0 };
        for part in text.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" => shortcut.ctrl = true,
                "alt" => shortcut.alt = true,
                "shift" => shortcut.shift = true,
                "win" => shortcut.win = true,
                _ => {
                    let (code, _) = key_names().into_iter().find(|(_, name)| name.eq_ignore_ascii_case(part))?;
                    shortcut.key = code;
                }
            }
        }
        (shortcut.key != 0).then_some(shortcut)
    }

    /// Whether this would make a sensible global shortcut: plain letters
    /// would fire while typing, so anything but a function key needs Ctrl,
    /// Alt or Win.
    pub fn check(&self) -> Result<(), &'static str> {
        if is_non_typing_key(self.key) || self.ctrl || self.alt || self.win {
            Ok(())
        } else {
            Err("Add Ctrl, Alt or Win (or use a function key, Pause or Scroll Lock), so typing doesn't trigger it.")
        }
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.win, "Win")] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        let name = key_names().into_iter().find(|(code, _)| *code == self.key).map(|(_, name)| name);
        write!(f, "{}", name.unwrap_or_else(|| format!("0x{:02X}", self.key)))
    }
}

#[cfg(windows)]
pub use imp::{HotkeyListener, held_shortcut};

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
        UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT, WM_USER,
    };

    use super::{Shortcut, key_names};

    fn down(vk: u16) -> bool {
        // SAFETY: reads the physical key state; no pointers involved.
        unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
    }

    /// The combination being pressed (call it from a key-press event, with
    /// the event's text), or `None` while only modifiers are held.
    ///
    /// Keys are read from what's physically held down. Pause is the
    /// exception: its press and release arrive together, so by now it reads
    /// as up; the event's text identifies it instead.
    pub fn held_shortcut(typed: &str) -> Option<Shortcut> {
        let held = key_names().into_iter().map(|(code, _)| code).find(|&code| down(code));
        let key = held.or_else(|| key_from_event(typed))?;
        Some(Shortcut { ctrl: down(0x11), alt: down(0x12), shift: down(0x10), win: down(0x5b) || down(0x5c), key })
    }

    /// Virtual-key code for keys Slint names but that may not read as held.
    fn key_from_event(typed: &str) -> Option<u16> {
        use slint::platform::Key;
        let typed = typed.chars().next()?;
        let named = [(Key::Pause, 0x13), (Key::ScrollLock, 0x91)];
        let function_keys = [
            Key::F1,
            Key::F2,
            Key::F3,
            Key::F4,
            Key::F5,
            Key::F6,
            Key::F7,
            Key::F8,
            Key::F9,
            Key::F10,
            Key::F11,
            Key::F12,
            Key::F13,
            Key::F14,
            Key::F15,
            Key::F16,
            Key::F17,
            Key::F18,
            Key::F19,
            Key::F20,
            Key::F21,
            Key::F22,
            Key::F23,
            Key::F24,
        ];
        let numbered = function_keys.into_iter().zip(0x70..);
        named.into_iter().chain(numbered).find(|(key, _)| char::from(*key) == typed).map(|(_, code)| code)
    }

    /// Owns a registered shortcut: a thread with a message loop receives it.
    /// Unregisters when dropped.
    pub struct HotkeyListener {
        thread_id: u32,
        thread: Option<JoinHandle<()>>,
    }

    impl HotkeyListener {
        /// `on_press` runs on the listener thread.
        pub fn start(shortcut: Shortcut, on_press: impl Fn() + Send + 'static) -> Result<Self, String> {
            let (ready_tx, ready_rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("hotkey".into())
                .spawn(move || listen(shortcut, &ready_tx, on_press))
                .map_err(|e| e.to_string())?;
            match ready_rx.recv() {
                Ok(Ok(thread_id)) => Ok(Self { thread_id, thread: Some(thread) }),
                Ok(Err(err)) => {
                    let _ = thread.join();
                    Err(err)
                }
                Err(_) => Err("the shortcut thread stopped".into()),
            }
        }
    }

    impl Drop for HotkeyListener {
        fn drop(&mut self) {
            // SAFETY: posting a plain message to our own thread's queue.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn listen(shortcut: Shortcut, ready: &mpsc::Sender<Result<u32, String>>, on_press: impl Fn()) {
        const ID: i32 = 1;
        let mut modifiers = MOD_NOREPEAT;
        for (on, flag) in [
            (shortcut.ctrl, MOD_CONTROL),
            (shortcut.alt, MOD_ALT),
            (shortcut.shift, MOD_SHIFT),
            (shortcut.win, MOD_WIN),
        ] {
            if on {
                modifiers |= flag;
            }
        }
        // SAFETY: Win32 calls on this thread with valid, owned arguments.
        unsafe {
            let mut msg = MSG::default();
            // Make sure the thread has a message queue before anyone posts to it.
            let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
            if let Err(err) = RegisterHotKey(None, ID, HOT_KEY_MODIFIERS(modifiers.0), shortcut.key as u32) {
                tracing::warn!(%shortcut, "couldn't register the shortcut: {err}");
                let _ = ready.send(Err(format!("{shortcut} is already used by another app. Pick another one.")));
                return;
            }
            let _ = ready.send(Ok(GetCurrentThreadId()));
            tracing::info!(%shortcut, "go-live shortcut registered");
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_HOTKEY {
                    on_press();
                }
            }
            let _ = UnregisterHotKey(None, ID);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_round_trip_through_text() {
        for text in
            ["Ctrl+Alt+S", "Ctrl+Shift+F9", "F13", "Alt+Win+Num5", "Ctrl+PageDown", "Shift+Win+0", "Shift+Pause"]
        {
            let shortcut = Shortcut::parse(text).unwrap();
            assert_eq!(shortcut.to_string(), text);
        }
        assert_eq!(Shortcut::parse("ctrl + alt + s"), Shortcut::parse("Ctrl+Alt+S"));
        assert!(Shortcut::parse("Ctrl+Alt").is_none(), "no key");
        assert!(Shortcut::parse("Ctrl+Ç").is_none(), "unknown key");
        assert!(Shortcut::parse("").is_none());
    }

    #[test]
    fn plain_keys_need_a_modifier() {
        assert!(Shortcut::parse("S").unwrap().check().is_err());
        assert!(Shortcut::parse("Shift+S").unwrap().check().is_err());
        assert!(Shortcut::parse("Ctrl+S").unwrap().check().is_ok());
        assert!(Shortcut::parse("F9").unwrap().check().is_ok());
        assert!(Shortcut::parse("Shift+Pause").unwrap().check().is_ok(), "Pause never types");
        assert!(Shortcut::parse("ScrollLock").unwrap().check().is_ok());
        assert!(Shortcut::parse("Ctrl+Break").unwrap().check().is_ok());
        assert!(Shortcut::parse("Shift+Insert").unwrap().check().is_err(), "that's paste");
    }
}
