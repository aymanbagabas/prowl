//! Copy text to the system clipboard with OSC 52, the terminal escape that asks
//! the emulator itself to set the clipboard. No dependency, no subprocess, and —
//! unlike `pbcopy`/`xclip` — it reaches the clipboard of the machine you're
//! *looking at*, so it works over SSH too. The terminal has to support it (and
//! tmux needs `set -g set-clipboard on`); when it doesn't, the sequence is
//! silently swallowed, which is why the dashboard's confirmation says the link
//! was sent rather than pasted.

use std::io::Write;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard (padded) base64 — OSC 52 carries its payload encoded, and this is
/// the only place the crate needs it, so it's 15 lines instead of a dependency.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b3 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b1 << 16) | (b2 << 8) | b3;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            // A 1- or 2-byte tail encodes to 2 or 3 characters, padded to four.
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Ask the terminal to put `text` on the clipboard (`c`, the selection every
/// emulator maps to the system clipboard). Written straight to stdout — it's a
/// control sequence, so it paints nothing and can't disturb the frame.
pub fn copy(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn base64_matches_the_standard_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The +/ end of the alphabet and a multi-byte character.
        assert_eq!(base64(&[0xff, 0xef, 0xbf]), "/++/");
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }

    #[test]
    fn base64_round_trips_a_markdown_list() {
        let list = "- https://github.com/o/r/pull/1\n- https://github.com/o/r/pull/2";
        let encoded = base64(list.as_bytes());
        assert!(!encoded.contains('\n'));
        // Decode with a tiny inverse to prove the encoder, not just its shape.
        let bits = encoded
            .chars()
            .filter(|c| *c != '=')
            .fold(String::new(), |mut acc, c| {
                let i = ALPHABET.iter().position(|a| *a as char == c).unwrap();
                let _ = write!(acc, "{i:06b}");
                acc
            });
        let decoded: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .filter(|c| c.len() == 8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        assert_eq!(String::from_utf8(decoded).unwrap(), list);
    }
}
