//! Named key → terminal byte sequence translation for `pty_sendkey`.
//!
//! Covers the keys an agent actually needs to drive TUIs and REPLs: control
//! chars, arrows, navigation, function keys, and `ctrl+<letter>`. Sequences
//! are the standard xterm defaults that `alacritty_terminal` and virtually
//! every program expect.

/// Translate one key name to the bytes to write to the PTY.
/// Returns `None` for unknown names.
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let n = name.trim().to_ascii_lowercase();
    let b: &[u8] = match n.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "tab" => b"\t",
        "backtab" | "shift+tab" => b"\x1b[Z",
        "escape" | "esc" => b"\x1b",
        "space" => b" ",
        "backspace" | "bs" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        "insert" | "ins" => b"\x1b[2~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        // Common control aliases.
        "ctrl+c" => b"\x03",
        "ctrl+d" => b"\x04",
        "ctrl+z" => b"\x1a",
        "ctrl+l" => b"\x0c",
        "ctrl+a" => b"\x01",
        "ctrl+e" => b"\x05",
        "ctrl+u" => b"\x15",
        "ctrl+k" => b"\x0b",
        "ctrl+w" => b"\x17",
        "ctrl+r" => b"\x12",
        _ => {
            // Generic `ctrl+<letter>` → control char.
            if let Some(rest) = n.strip_prefix("ctrl+")
                && rest.len() == 1
                && let c = rest.as_bytes()[0]
                && c.is_ascii_alphabetic()
            {
                return Some(vec![c.to_ascii_uppercase() - b'A' + 1]);
            }
            return None;
        }
    };
    Some(b.to_vec())
}

/// Human-readable list of supported key names, for the tool description.
pub const SUPPORTED: &str = "enter, tab, backtab, escape, space, backspace, delete, \
up, down, left, right, home, end, pageup, pagedown, insert, f1-f12, \
and any ctrl+<letter> (e.g. ctrl+c, ctrl+d, ctrl+z)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_chars() {
        assert_eq!(key_bytes("ctrl+c").unwrap(), vec![0x03]);
        assert_eq!(key_bytes("ctrl+d").unwrap(), vec![0x04]);
        // Generic path matches the explicit alias.
        assert_eq!(key_bytes("ctrl+x").unwrap(), vec![0x18]);
        assert_eq!(key_bytes("CTRL+A").unwrap(), vec![0x01]);
    }

    #[test]
    fn named_keys() {
        assert_eq!(key_bytes("enter").unwrap(), b"\r");
        assert_eq!(key_bytes("up").unwrap(), b"\x1b[A");
        assert_eq!(key_bytes("f5").unwrap(), b"\x1b[15~");
    }

    #[test]
    fn unknown_is_none() {
        assert!(key_bytes("banana").is_none());
        assert!(key_bytes("ctrl+shift+x").is_none());
    }
}
