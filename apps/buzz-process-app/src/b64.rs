//! Base64 (RFC 4648 §4, standard alphabet) — needed to carry NIP-98 authorization
//! events in an HTTP header, and vendored for the same reason as the crypto: the
//! crate may name exactly one dependency.
//!
//! Decoding is strict. A permissive decoder is a security problem in an auth
//! path: if two distinct header strings decode to the same bytes, a replay guard
//! keyed on the header cannot do its job. So non-alphabet bytes are refused
//! rather than skipped, padding must be well-formed, and the unused bits of the
//! final quantum must be zero.

/// Encode with padding.
#[must_use]
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let sextet = |shift: u32| symbol(((n >> shift) & 0x3f) as u8);
        out.push(sextet(18));
        out.push(sextet(12));
        if chunk.len() > 1 {
            out.push(sextet(6));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(sextet(0));
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode, strictly. Returns `None` on any malformed input.
///
/// Also accepts the URL-safe alphabet (`-`/`_`) because clients and proxies
/// sometimes produce it, but never accepts whitespace, missing padding, or
/// non-zero trailing bits.
#[must_use]
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunks = bytes.chunks_exact(4);
    let total_chunks = bytes.len() / 4;
    for (i, chunk) in chunks.by_ref().enumerate() {
        let is_last = i + 1 == total_chunks;
        let c0 = *chunk.first()?;
        let c1 = *chunk.get(1)?;
        let c2 = *chunk.get(2)?;
        let c3 = *chunk.get(3)?;
        // Padding may only appear in the final quantum, and only in the last two
        // positions.
        if !is_last && (c2 == b'=' || c3 == b'=') {
            return None;
        }
        if c0 == b'=' || c1 == b'=' {
            return None;
        }
        let v0 = val(c0)?;
        let v1 = val(c1)?;
        match (c2, c3) {
            (b'=', b'=') => {
                // One output byte; the low 4 bits of v1 must be zero.
                if v1 & 0x0f != 0 {
                    return None;
                }
                out.push((v0 << 2) | (v1 >> 4));
            }
            (_, b'=') => {
                let v2 = val(c2)?;
                // Two output bytes; the low 2 bits of v2 must be zero.
                if v2 & 0x03 != 0 {
                    return None;
                }
                out.push((v0 << 2) | (v1 >> 4));
                out.push((v1 << 4) | (v2 >> 2));
            }
            _ => {
                let v2 = val(c2)?;
                let v3 = val(c3)?;
                out.push((v0 << 2) | (v1 >> 4));
                out.push((v1 << 4) | (v2 >> 2));
                out.push((v2 << 6) | v3);
            }
        }
    }
    Some(out)
}

/// The alphabet symbol for a 6-bit value. Arithmetic rather than a table lookup
/// so there is no index to prove in bounds; `v` is masked by the caller.
const fn symbol(v: u8) -> char {
    match v & 0x3f {
        n @ 0..=25 => (b'A' + n) as char,
        n @ 26..=51 => (b'a' + n - 26) as char,
        n @ 52..=61 => (b'0' + n - 52) as char,
        62 => '+',
        _ => '/',
    }
}

const fn val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_round_trip() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), encoded, "encoding {raw:?}");
            assert_eq!(
                decode(encoded).as_deref(),
                Some(raw.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn binary_round_trips() {
        let all: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        for len in 0..40usize {
            let v: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            assert_eq!(decode(&encode(&v)).unwrap(), v, "length {len}");
        }
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in [
            "Zg=",       // wrong length
            "Zg",        // wrong length
            "Zm9v YmFy", // whitespace
            "Zm9v\nYmFy",
            "Zg=x", // data after padding
            "=g==", // padding first
            "Z===", // too much padding
            "Zm9!", // non-alphabet
            "Zm9vYmF",
        ] {
            assert!(decode(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn non_canonical_trailing_bits_are_refused() {
        // "Zh==" would decode to the same byte as "Zg==" if trailing bits were
        // ignored; accepting both would give one byte string two header spellings
        // and defeat a replay guard keyed on the header.
        assert!(decode("Zg==").is_some());
        assert!(decode("Zh==").is_none());
        assert!(decode("Zm8=").is_some());
        assert!(decode("Zm9=").is_none());
    }

    #[test]
    fn url_safe_alphabet_is_accepted() {
        let raw = [0xfbu8, 0xff, 0xbf];
        let std = encode(&raw);
        assert!(std.contains('+') || std.contains('/'));
        let urlsafe: String = std
            .chars()
            .map(|c| match c {
                '+' => '-',
                '/' => '_',
                other => other,
            })
            .collect();
        assert_eq!(decode(&urlsafe).unwrap(), raw);
    }
}
