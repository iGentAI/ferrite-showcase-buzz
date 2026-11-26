//! bech32, for the one place this shell shows a key to a person.
//!
//! An agent's identity is the thing its whole history hangs off, so the client's
//! creation dialog offers it for backup as an `nsec` — and `nsec` means bech32,
//! not hex: it is what a person can paste into another client. Encode only,
//! because nothing here ever reads one back.
//!
//! Transcribed from BIP-173 rather than taken from a crate, for the reason this
//! crate takes almost no dependencies — and pinned by NIP-19's own published test
//! vector, which is what makes "transcribed correctly" a checkable claim rather
//! than a hope.

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// The BIP-173 generator, over GF(2^5)-coded polynomials.
const GENERATOR: [u32; 5] = [
    0x3b6a_57b2,
    0x2650_8e6d,
    0x1ea1_19fa,
    0x3d42_33dd,
    0x2a14_62b3,
];

fn polymod(values: &[u8]) -> u32 {
    let mut checksum: u32 = 1;
    for value in values {
        let top = checksum >> 25;
        checksum = ((checksum & 0x01ff_ffff) << 5) ^ u32::from(*value);
        for (bit, generator) in GENERATOR.iter().enumerate() {
            if (top >> bit) & 1 == 1 {
                checksum ^= generator;
            }
        }
    }
    checksum
}

/// The human-readable part, expanded as the checksum consumes it: high bits,
/// a separator, then low bits.
fn expand_hrp(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|byte| byte >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|byte| byte & 31));
    out
}

/// Regroup 8-bit bytes into 5-bit values, padding the last group.
///
/// The padding is what makes 32 bytes become 52 values rather than 51.2: the
/// remaining bits are shifted up and zero-filled, exactly as BIP-173 specifies.
fn to_five_bit(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 8 / 5 + 1);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in data {
        accumulator = (accumulator << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((accumulator >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((accumulator << (5 - bits)) & 31) as u8);
    }
    out
}

/// Encode `data` under `hrp`.
///
/// `None` for an unusable human-readable part rather than a malformed string: a
/// value a person is meant to copy has to be right or absent.
#[must_use]
pub fn encode(hrp: &str, data: &[u8]) -> Option<String> {
    if hrp.is_empty() || hrp.len() > 83 {
        return None;
    }
    if !hrp.bytes().all(|byte| (33..=126).contains(&byte) && !byte.is_ascii_uppercase()) {
        return None;
    }
    let values = to_five_bit(data);
    // BIP-173 caps a bech32 string at 90 characters, so a longer one is simply not
    // one — a conformant decoder is entitled to refuse it. Checked on the TOTAL,
    // because the human-readable part and the checksum count toward it: a bound on
    // the data alone would let a long prefix push a valid-looking string over.
    let total = hrp.len() + 1 + values.len() + 6;
    if total > 90 {
        return None;
    }
    let mut checksum_input = expand_hrp(hrp);
    checksum_input.extend(values.iter().copied());
    checksum_input.extend([0u8; 6]);
    // The trailing XOR with 1 is bech32's constant — bech32m's is 0x2bc830a3, and
    // NIP-19 uses bech32.
    let checksum = polymod(&checksum_input) ^ 1;
    // Every value is five bits (`to_five_bit` masks, and the checksum groups
    // are `& 31`), so the charset lookup cannot miss.
    let mut out = String::with_capacity(total);
    out.push_str(hrp);
    out.push('1');
    for value in &values {
        out.push(char::from(CHARSET[usize::from(*value)]));
    }
    for group in 0..6 {
        let value = ((checksum >> (5 * (5 - group))) & 31) as u8;
        out.push(char::from(CHARSET[usize::from(value)]));
    }
    Some(out)
}

#[must_use]
pub fn nsec(secret: &[u8; 32]) -> Option<String> {
    encode("nsec", secret)
}

#[must_use]
pub fn npub(pubkey: &[u8; 32]) -> Option<String> {
    encode("npub", pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_hex(hex: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16).expect("hex");
        }
        out
    }

    #[test]
    fn nip19s_own_vectors_encode_exactly() {
        // Published in NIP-19 itself, so this test checks the transcription
        // against the specification rather than against this implementation's
        // own output.
        assert_eq!(
            nsec(&from_hex(
                "67dea2ed018072d675f5415ecfaed7d2597555e202d85b3d65ea4e58d2d92ffa"
            ))
            .expect("nsec"),
            "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5"
        );
        assert_eq!(
            npub(&from_hex(
                "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
            ))
            .expect("npub"),
            "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg"
        );
    }

    #[test]
    fn the_prefix_is_inside_the_checksum() {
        // An npub of the same bytes differs after the prefix, which is what stops
        // one being read as the other.
        let encoded = nsec(&[7u8; 32]).expect("nsec");
        assert_ne!(encoded[5..], npub(&[7u8; 32]).expect("npub")[5..]);
    }

    #[test]
    fn an_unusable_prefix_is_refused_rather_than_mangled() {
        assert!(encode("", &[0u8; 32]).is_none());
        assert!(encode("NSEC", &[0u8; 32]).is_none(), "bech32 hrps are lowercase");
        assert!(encode("ns ec", &[0u8; 32]).is_none());
        assert!(encode(&"x".repeat(84), &[0u8; 32]).is_none());
    }

    #[test]
    fn a_string_over_bech32s_own_length_limit_is_refused() {
        // 90 characters is the specification's cap, so anything longer is not a
        // bech32 string at all — and this encoder is public and generic, so the
        // bound cannot be left implicit in "we only ever encode 32 bytes".
        assert!(encode("nsec", &[0u8; 32]).is_some(), "a key fits comfortably");
        assert!(encode("x", &[0u8; 51]).is_some(), "82 + 1 + 1 + 6 = 90 exactly");
        assert!(encode("x", &[0u8; 52]).is_none(), "one group over");
        // A long prefix pushes an otherwise-fine payload over, which is why the
        // check is on the total rather than on the data.
        assert!(encode(&"x".repeat(40), &[0u8; 32]).is_none());
    }
}
