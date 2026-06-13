use remux_core::{RemuxError, Request, SessionSelector};

use crate::client::RemuxClient;

/// Mutually-exclusive input sources for `remux send`.
pub enum InputSource {
    /// Send the string's bytes, interpreting only `\n`, `\t`, `\r`, `\\`.
    Text(String),
    /// Decode a hex string (e.g. "1b5b41") into raw bytes.
    BytesHex(String),
    /// Map a named key (e.g. "Enter", "Up") to bytes.
    Key(String),
    /// Read all of stdin as raw bytes.
    Stdin,
}

/// Handle the `send` command. Sends input to a session's PTY without attaching
/// (fire-and-forget). The daemon sends no response for `SendInput`, so we do
/// not block waiting for one.
pub async fn run(
    client: &mut RemuxClient,
    name: String,
    source: InputSource,
) -> Result<(), RemuxError> {
    let session = parse_selector(&name);
    let data = resolve_input(source)?;

    // Fire-and-forget: the daemon does not reply to SendInput. We write the
    // request and return without awaiting a response.
    client
        .send_oneway(Request::SendInput { session, data })
        .await?;

    Ok(())
}

/// Parse a session name or ID into a SessionSelector.
fn parse_selector(name: &str) -> SessionSelector {
    if let Ok(uuid) = uuid::Uuid::parse_str(name) {
        SessionSelector::Id(remux_core::SessionId(uuid))
    } else {
        SessionSelector::Name(name.to_string())
    }
}

/// Resolve the selected input source into the raw bytes to send.
fn resolve_input(source: InputSource) -> Result<Vec<u8>, RemuxError> {
    match source {
        InputSource::Text(s) => Ok(decode_text_escapes(&s)),
        InputSource::BytesHex(h) => decode_hex(&h),
        InputSource::Key(name) => key_to_bytes(&name),
        InputSource::Stdin => read_stdin(),
    }
}

/// Decode the limited escape set for `--text`. ONLY these escapes are
/// interpreted (binary-safe, no shell or other interpretation):
///   `\n` -> 0x0A, `\t` -> 0x09, `\r` -> 0x0D, `\\` -> `\`.
/// A backslash followed by any other character is passed through verbatim
/// (both the backslash and the character).
fn decode_text_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push(b'\n');
                    chars.next();
                }
                Some('t') => {
                    out.push(b'\t');
                    chars.next();
                }
                Some('r') => {
                    out.push(b'\r');
                    chars.next();
                }
                Some('\\') => {
                    out.push(b'\\');
                    chars.next();
                }
                // Unknown escape: emit the backslash verbatim; the following
                // character is handled on the next loop iteration.
                _ => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice('\\'.encode_utf8(&mut buf).as_bytes());
                }
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// Decode a hex string (no separators, even length) into raw bytes.
fn decode_hex(h: &str) -> Result<Vec<u8>, RemuxError> {
    let h = h.trim();
    if !h.len().is_multiple_of(2) {
        return Err(RemuxError::InvalidRequest(format!(
            "invalid hex: odd number of digits ({} chars)",
            h.len()
        )));
    }
    let bytes = h.as_bytes();
    let mut out = Vec::with_capacity(h.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Result<u8, RemuxError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(RemuxError::InvalidRequest(format!(
            "invalid hex digit: {:?}",
            other as char
        ))),
    }
}

/// Map a common key name to the bytes a terminal emits for it.
fn key_to_bytes(name: &str) -> Result<Vec<u8>, RemuxError> {
    let bytes: &[u8] = match name {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Esc" => &[0x1b],
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Backspace" => &[0x7f],
        "Space" => b" ",
        other => {
            return Err(RemuxError::InvalidRequest(format!(
                "unknown key name: {other:?} (known: Enter, Tab, Esc, Up, Down, Right, Left, Backspace, Space)"
            )));
        }
    };
    Ok(bytes.to_vec())
}

/// Read all of stdin as raw bytes.
fn read_stdin() -> Result<Vec<u8>, RemuxError> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| RemuxError::IoError(format!("failed to read stdin: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_interpreted() {
        assert_eq!(decode_text_escapes("a\\nb"), b"a\nb");
        assert_eq!(decode_text_escapes("\\t"), b"\t");
        assert_eq!(decode_text_escapes("\\r"), b"\r");
        assert_eq!(decode_text_escapes("a\\\\b"), b"a\\b");
    }

    #[test]
    fn text_no_other_escapes() {
        // \x is not interpreted; backslash and x pass through verbatim.
        assert_eq!(decode_text_escapes("\\x41"), b"\\x41");
        // Trailing backslash passes through.
        assert_eq!(decode_text_escapes("ab\\"), b"ab\\");
    }

    #[test]
    fn text_is_binary_safe_utf8() {
        assert_eq!(decode_text_escapes("héllo"), "héllo".as_bytes());
    }

    #[test]
    fn hex_decodes() {
        assert_eq!(decode_hex("1b5b41").unwrap(), vec![0x1b, 0x5b, 0x41]);
        assert_eq!(decode_hex("00FF").unwrap(), vec![0x00, 0xff]);
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_rejects_invalid() {
        assert!(decode_hex("1b5").is_err()); // odd length
        assert!(decode_hex("zz").is_err()); // bad digit
        assert!(decode_hex("1g").is_err()); // bad digit
    }

    #[test]
    fn keys_map() {
        assert_eq!(key_to_bytes("Enter").unwrap(), b"\r");
        assert_eq!(key_to_bytes("Tab").unwrap(), b"\t");
        assert_eq!(key_to_bytes("Esc").unwrap(), vec![0x1b]);
        assert_eq!(key_to_bytes("Up").unwrap(), b"\x1b[A");
        assert_eq!(key_to_bytes("Down").unwrap(), b"\x1b[B");
        assert_eq!(key_to_bytes("Right").unwrap(), b"\x1b[C");
        assert_eq!(key_to_bytes("Left").unwrap(), b"\x1b[D");
        assert_eq!(key_to_bytes("Backspace").unwrap(), vec![0x7f]);
        assert_eq!(key_to_bytes("Space").unwrap(), b" ");
    }

    #[test]
    fn keys_reject_unknown() {
        assert!(key_to_bytes("Foo").is_err());
        assert!(key_to_bytes("enter").is_err()); // case-sensitive
    }
}
