//! Bech32 / bech32m, implemented from BIP-173 and BIP-350.
//!
//! Provenance: <https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki>
//! (bech32, checksum constant 1) and
//! <https://github.com/bitcoin/bips/blob/master/bip-0350.mediawiki>
//! (bech32m, checksum constant `0x2bc830a3`). Vendored because the platform
//! code gate admits exactly one dependency, so the `bech32` crate cannot be
//! linked.
//!
//! Scope is Nostr's NIP-19 *bare* display encodings — `npub`, `nsec`, `note` —
//! which are always bech32 (never bech32m) over exactly 32 bytes of payload.
//! The TLV forms (`nprofile`, `nevent`, …) are deliberately out of scope: this
//! module's [`encode`]/[`decode`] pair is fixed at a 32-byte payload so a
//! caller can never accidentally accept a truncated key.
//!
//! Everything here is public-data-only (keys as published, event ids), so the
//! implementation is straightforward table-driven code rather than the
//! constant-time discipline used in [`super::schnorr`].

/// The bech32 data charset (BIP-173): value `v` encodes to `CHARSET[v]`.
const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// BCH generator coefficients for the bech32 checksum (BIP-173).
const GENERATOR: [u32; 5] = [
    0x3b6a_57b2,
    0x2650_8e6d,
    0x1ea1_19fa,
    0x3d42_33dd,
    0x2a14_62b3,
];

/// Maximum total length of a bech32 string (BIP-173). NIP-19 bare forms are
/// far shorter than this (`npub` is 63 characters), but the limit is part of
/// the checksum's error-detection guarantee, so it is enforced.
const MAX_LEN: usize = 90;

/// Minimum total length: one HRP character, the separator, and six checksum
/// characters.
const MIN_LEN: usize = 8;

/// Number of 5-bit groups a 32-byte payload occupies: `ceil(256 / 5)`, the
/// last group carrying four bits of zero padding.
const DATA_GROUPS_32: usize = 52;

/// Which checksum constant a string uses.
///
/// The two variants differ only in the final XOR applied to the checksum, so a
/// bech32 string never validates as bech32m and vice versa (that separation is
/// the entire point of BIP-350).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// BIP-173 bech32: checksum constant 1. Nostr's NIP-19 encodings.
    Bech32,
    /// BIP-350 bech32m: checksum constant `0x2bc830a3`.
    Bech32m,
}

impl Variant {
    /// The constant the checksum polynomial is XORed with.
    const fn constant(self) -> u32 {
        match self {
            Self::Bech32 => 1,
            Self::Bech32m => 0x2bc8_30a3,
        }
    }
}

/// Encode 32 bytes as a lowercase bech32 (BIP-173) string under `hrp`.
///
/// The HRP must be non-empty, all-lowercase, and printable US-ASCII
/// (`0x21..=0x7e`); uppercase input is rejected rather than folded, because a
/// caller passing `"NPUB"` is more likely confused than requesting the
/// uppercase encoding. Returns `None` if the HRP is invalid or if the total
/// length would exceed [`MAX_LEN`] (HRP longer than 31 characters).
///
/// Output length is always `hrp.len() + 59`.
#[must_use]
pub fn encode(hrp: &str, data32: &[u8; 32]) -> Option<String> {
    encode_variant(hrp, data32, Variant::Bech32)
}

/// Encode 32 bytes under `hrp` with an explicit checksum [`Variant`].
///
/// Same contract as [`encode`]; exposed so bech32m payloads (BIP-350) can be
/// produced without a second implementation of the regrouping and checksum.
#[must_use]
pub fn encode_variant(hrp: &str, data32: &[u8; 32], variant: Variant) -> Option<String> {
    let hrp_bytes = hrp.as_bytes();
    if hrp_bytes.is_empty() {
        return None;
    }
    for &c in hrp_bytes {
        // Printable ASCII only, and no uppercase: mixed case is unrepresentable
        // in a lowercase output, so uppercase is refused outright.
        if !(0x21..=0x7e).contains(&c) || c.is_ascii_uppercase() {
            return None;
        }
    }
    if hrp_bytes.len() + 1 + DATA_GROUPS_32 + 6 > MAX_LEN {
        return None;
    }

    let data = convert_bits(data32, 8, 5, true)?;
    let checksum = create_checksum(hrp_bytes, &data, variant);

    let mut out = String::with_capacity(hrp_bytes.len() + 1 + data.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for v in data.iter().chain(checksum.iter()) {
        out.push(charset_char(*v)?);
    }
    Some(out)
}

/// Decode a bech32 (BIP-173) string whose payload is exactly 32 bytes,
/// returning the lowercased HRP and the payload.
///
/// Rejects, with `None`: a bad checksum, any character outside the bech32
/// charset, a character outside printable ASCII in the HRP, mixed upper/lower
/// case anywhere in the string, a missing `'1'` separator, a separator leaving
/// an empty HRP or fewer than six data characters, a total length outside
/// `8..=90`, non-zero or over-long bit padding, and any payload that is not
/// exactly 32 bytes.
///
/// Input may be entirely uppercase (BIP-173 permits it for QR efficiency); the
/// returned HRP is always lowercase.
#[must_use]
pub fn decode(s: &str) -> Option<(String, [u8; 32])> {
    let (hrp, data) = decode_raw(s, Variant::Bech32)?;
    if data.len() != DATA_GROUPS_32 {
        return None;
    }
    let bytes = convert_bits(&data, 5, 8, false)?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some((hrp, out))
}

/// Encode an x-only BIP-340 public key as a NIP-19 `npub`.
///
/// Never fails for a well-formed key: `"npub"` is a valid HRP, so the `Option`
/// exists only to keep the fallible-encode contract uniform.
#[must_use]
pub fn npub(pubkey_x: &[u8; 32]) -> Option<String> {
    encode("npub", pubkey_x)
}

/// Encode a 32-byte event id as a NIP-19 `note`.
#[must_use]
pub fn note(event_id: &[u8; 32]) -> Option<String> {
    encode("note", event_id)
}

/// Decode a NIP-19 `npub` into an x-only public key.
///
/// Returns `None` unless the string is a valid bech32 encoding *and* its HRP is
/// exactly `npub` — a `nsec` or `note` string of the right shape is refused, so
/// this cannot be used to smuggle a secret key into a pubkey field.
#[must_use]
pub fn decode_npub(s: &str) -> Option<[u8; 32]> {
    let (hrp, data) = decode(s)?;
    if hrp != "npub" {
        return None;
    }
    Some(data)
}

/// Validate a bech32/bech32m string and return `(lowercase hrp, 5-bit data)`
/// with the six checksum characters stripped. The payload length is *not*
/// constrained here; [`decode`] applies the 32-byte rule.
fn decode_raw(s: &str, variant: Variant) -> Option<(String, Vec<u8>)> {
    let bytes = s.as_bytes();
    if bytes.len() < MIN_LEN || bytes.len() > MAX_LEN {
        return None;
    }

    let mut has_lower = false;
    let mut has_upper = false;
    for &c in bytes {
        if !(0x21..=0x7e).contains(&c) {
            return None;
        }
        if c.is_ascii_lowercase() {
            has_lower = true;
        }
        if c.is_ascii_uppercase() {
            has_upper = true;
        }
    }
    if has_lower && has_upper {
        return None;
    }

    let lower = s.to_ascii_lowercase();
    // The separator is the *last* '1': 'q'..'l' charset excludes '1', so any
    // earlier '1' belongs to the HRP.
    let sep = lower.rfind('1')?;
    let hrp = lower.get(..sep)?;
    let data_part = lower.get(sep + 1..)?;
    if hrp.is_empty() || data_part.len() < 6 {
        return None;
    }

    let mut values = Vec::with_capacity(data_part.len());
    for &c in data_part.as_bytes() {
        values.push(charset_value(c)?);
    }
    if !verify_checksum(hrp.as_bytes(), &values, variant) {
        return None;
    }
    values.truncate(values.len() - 6);
    Some((hrp.to_string(), values))
}

/// The bech32 BCH polynomial modulo step over a stream of 5-bit values.
fn polymod<I: IntoIterator<Item = u8>>(values: I) -> u32 {
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(v);
        for (i, g) in GENERATOR.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= *g;
            }
        }
    }
    chk
}

/// HRP expansion (BIP-173): high 3 bits of each HRP byte, a zero separator,
/// then the low 5 bits of each HRP byte.
fn hrp_expand(hrp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hrp.len() * 2 + 1);
    out.extend(hrp.iter().map(|c| c >> 5));
    out.push(0);
    out.extend(hrp.iter().map(|c| c & 0x1f));
    out
}

/// Six 5-bit checksum values for `data` under `hrp`.
fn create_checksum(hrp: &[u8], data: &[u8], variant: Variant) -> [u8; 6] {
    let values = hrp_expand(hrp)
        .into_iter()
        .chain(data.iter().copied())
        .chain([0u8; 6]);
    let poly = polymod(values) ^ variant.constant();
    let mut out = [0u8; 6];
    for (i, slot) in out.iter_mut().enumerate() {
        // Shift is 25, 20, ... 0; `i < 6` so the shift is always < 32.
        let shift = 5 * (5 - i);
        *slot = ((poly >> shift) & 0x1f) as u8;
    }
    out
}

/// Whether `data` (payload **plus** its six checksum values) checks out.
fn verify_checksum(hrp: &[u8], data: &[u8], variant: Variant) -> bool {
    let values = hrp_expand(hrp).into_iter().chain(data.iter().copied());
    polymod(values) == variant.constant()
}

/// Regroup `data` from `from`-bit to `to`-bit groups, most-significant bit
/// first.
///
/// With `pad = true` a trailing partial group is zero-padded out (the encode
/// direction). With `pad = false` any leftover bits must be fewer than `to`
/// and all zero, which is the BIP-173 rule that makes the 8→5 encoding
/// injective; violating it returns `None`.
fn convert_bits(data: &[u8], from: u32, to: u32, pad: bool) -> Option<Vec<u8>> {
    if from == 0 || from > 8 || to == 0 || to > 8 {
        return None;
    }
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let max = (1u32 << to) - 1;
    let mut out = Vec::with_capacity(data.len() * from as usize / to as usize + 1);
    for &v in data {
        let v = u32::from(v);
        if from < 8 && v >= (1u32 << from) {
            return None;
        }
        acc = (acc << from) | v;
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & max) as u8);
        }
    }
    if pad {
        if bits > 0 {
            out.push(((acc << (to - bits)) & max) as u8);
        }
    } else if bits >= from || ((acc << (to - bits)) & max) != 0 {
        return None;
    }
    Some(out)
}

/// Charset character for a 5-bit value; `None` if `v > 31`.
fn charset_char(v: u8) -> Option<char> {
    CHARSET.get(usize::from(v)).map(|b| char::from(*b))
}

/// 5-bit value for a charset character; `None` for anything outside it.
/// `'1'`, `'b'`, `'i'` and `'o'` are excluded by construction.
fn charset_value(c: u8) -> Option<u8> {
    let pos = CHARSET.iter().position(|x| *x == c)?;
    u8::try_from(pos).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::crypto::{from_hex32, to_hex};

    /// BIP-173 valid checksum vectors. They carry arbitrary payload lengths, so
    /// they exercise `decode_raw` rather than the 32-byte `decode`.
    const BIP173_VALID: &[&str] = &[
        "A12UEL5L",
        "a12uel5l",
        "an83characterlonghumanreadablepartthatcontainsthenumber1andtheexcludedcharactersbio1tt5tgs",
        "abcdef1qpzry9x8gf2tvdw0s3jn54khce6mua7lmqqqxw",
        "11qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqc8247j",
        "split1checkupstagehandshakeupstreamerranterredcaperred2y9e3w",
        "?1ezyfcl",
    ];

    #[test]
    fn bip173_valid_vectors_pass_the_checksum() {
        for s in BIP173_VALID {
            let (hrp, data) =
                decode_raw(s, Variant::Bech32).unwrap_or_else(|| panic!("should validate: {s}"));
            assert!(!hrp.is_empty());
            assert_eq!(hrp, hrp.to_ascii_lowercase());
            // Total length = hrp + separator + data + checksum.
            assert_eq!(s.len(), hrp.len() + 1 + data.len() + 6);
            assert!(data.iter().all(|v| *v < 32));
        }
    }

    #[test]
    fn bip173_valid_vectors_reject_the_bech32m_constant() {
        for s in BIP173_VALID {
            assert!(
                decode_raw(s, Variant::Bech32m).is_none(),
                "bech32 must not validate as bech32m: {s}"
            );
        }
    }

    #[test]
    fn bip173_invalid_vectors_are_rejected() {
        // HRP character out of printable-ASCII range (0x20 space, 0x7f DEL).
        assert!(decode_raw("\x201nwldj5", Variant::Bech32).is_none());
        assert!(decode_raw("\x7f1axkwrx", Variant::Bech32).is_none());
        // Overall max length exceeded.
        assert!(decode_raw(
            "an84characterslonghumanreadablepartthatcontainsthenumber1andtheexcludedcharactersbio1569pvx",
            Variant::Bech32
        )
        .is_none());
        // No separator.
        assert!(decode_raw("pzry9x0s0muk", Variant::Bech32).is_none());
        // Empty HRP.
        assert!(decode_raw("1pzry9x0s0muk", Variant::Bech32).is_none());
        assert!(decode_raw("10a06t8", Variant::Bech32).is_none());
        assert!(decode_raw("1qzzfhee", Variant::Bech32).is_none());
        // Invalid data character ('b' is not in the charset).
        assert!(decode_raw("x1b4n0q5v", Variant::Bech32).is_none());
        // Too short a checksum.
        assert!(decode_raw("li1dgmt3", Variant::Bech32).is_none());
        // Invalid character in the checksum (0xff byte, non-ASCII).
        assert!(decode_raw("de1lg7wt\u{ff}", Variant::Bech32).is_none());
        // Checksum computed over an uppercase HRP but string given lowercase.
        assert!(decode_raw("A1G7SGD8", Variant::Bech32).is_none());
    }

    #[test]
    fn mixed_case_is_rejected() {
        // Both all-lower and all-upper forms of the same string validate.
        assert!(decode_raw("a12uel5l", Variant::Bech32).is_some());
        assert!(decode_raw("A12UEL5L", Variant::Bech32).is_some());
        // Any mixture does not.
        assert!(decode_raw("A12UeL5L", Variant::Bech32).is_none());
        assert!(decode_raw("a12UEL5L", Variant::Bech32).is_none());

        let key = [0x11u8; 32];
        let encoded = npub(&key).unwrap();
        let upper = encoded.to_ascii_uppercase();
        assert_eq!(decode_npub(&upper), Some(key));
        let mut mixed = upper.clone();
        mixed.replace_range(0..1, "n");
        assert!(decode_npub(&mixed).is_none());
    }

    #[test]
    fn npub_round_trips_for_fixed_keys() {
        let vectors: [[u8; 32]; 5] = [
            [0x00; 32],
            [0xff; 32],
            [0x01; 32],
            {
                let mut k = [0u8; 32];
                for (i, b) in k.iter_mut().enumerate() {
                    *b = i as u8;
                }
                k
            },
            from_hex32("82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2").unwrap(),
        ];
        for key in vectors {
            let encoded = npub(&key).unwrap();
            assert!(encoded.starts_with("npub1"), "{encoded}");
            // 4 HRP + 1 separator + 52 data + 6 checksum.
            assert_eq!(encoded.len(), 63);
            assert_eq!(encoded, encoded.to_ascii_lowercase());
            assert_eq!(decode_npub(&encoded), Some(key), "hex {}", to_hex(&key));
            assert_eq!(decode(&encoded), Some(("npub".to_string(), key)));
        }
    }

    /// An external NIP-19 vector: fiatjaf's published identity. Both the
    /// 52 payload characters and the 6 checksum characters were produced
    /// independently of this code, so agreement pins the regrouping *and* the
    /// checksum against the wider Nostr ecosystem rather than against
    /// ourselves.
    #[test]
    fn known_nostr_npub_vector() {
        const HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
        const NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";

        let key = from_hex32(HEX).unwrap();
        assert_eq!(npub(&key).as_deref(), Some(NPUB));
        assert_eq!(decode_npub(NPUB), Some(key));
        assert_eq!(to_hex(&decode_npub(NPUB).unwrap()), HEX);
    }

    #[test]
    fn note_and_npub_are_distinct_namespaces() {
        let id = [0x5au8; 32];
        let n = note(&id).unwrap();
        assert!(n.starts_with("note1"));
        assert_eq!(decode(&n), Some(("note".to_string(), id)));
        // A `note` string is not accepted as an npub even though the payload
        // is a valid 32-byte value.
        assert!(decode_npub(&n).is_none());
    }

    #[test]
    fn single_character_mutation_breaks_the_checksum() {
        let key =
            from_hex32("7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e").unwrap();
        let encoded = npub(&key).unwrap();
        assert!(decode(&encoded).is_some());

        let original = encoded.clone().into_bytes();
        for pos in 0..original.len() {
            for &c in CHARSET.iter() {
                let mut m = original.clone();
                if m[pos] == c {
                    continue;
                }
                m[pos] = c;
                let candidate = String::from_utf8(m).unwrap();
                assert!(
                    decode(&candidate).is_none(),
                    "single-character mutation at {pos} still decoded: {candidate}"
                );
            }
        }
    }

    #[test]
    fn payload_must_be_exactly_32_bytes() {
        // 20-byte payload with a valid bech32 checksum (a BIP-173 vector-shaped
        // string): structurally fine, but not a NIP-19 bare key.
        let short = {
            let data = convert_bits(&[0xabu8; 20], 8, 5, true).unwrap();
            let checksum = create_checksum(b"npub", &data, Variant::Bech32);
            let mut s = String::from("npub1");
            for v in data.iter().chain(checksum.iter()) {
                s.push(charset_char(*v).unwrap());
            }
            s
        };
        assert!(decode_raw(&short, Variant::Bech32).is_some());
        assert!(decode(&short).is_none());
        assert!(decode_npub(&short).is_none());
    }

    #[test]
    fn non_zero_padding_is_rejected() {
        // 52 groups carry 260 bits; the low four bits of the last group are
        // padding and must be zero. Flip one of them and keep the checksum
        // valid: decode must still refuse.
        let key = [0x42u8; 32];
        let mut data = convert_bits(&key, 8, 5, true).unwrap();
        assert_eq!(data.len(), DATA_GROUPS_32);
        let last = data.len() - 1;
        assert_eq!(data[last] & 0x0f, 0);
        data[last] |= 0x01;
        let checksum = create_checksum(b"npub", &data, Variant::Bech32);
        let mut s = String::from("npub1");
        for v in data.iter().chain(checksum.iter()) {
            s.push(charset_char(*v).unwrap());
        }
        assert!(
            decode_raw(&s, Variant::Bech32).is_some(),
            "checksum should be valid"
        );
        assert!(decode(&s).is_none(), "non-zero padding must be rejected");
    }

    #[test]
    fn encode_rejects_bad_hrps() {
        let key = [0u8; 32];
        assert!(encode("", &key).is_none());
        assert!(encode("NPUB", &key).is_none());
        assert!(encode("np ub", &key).is_none());
        assert!(encode("np\u{ff}ub", &key).is_none());
        // 32 characters of HRP would push the total past 90.
        assert!(encode(&"a".repeat(31), &key).is_some());
        assert!(encode(&"a".repeat(32), &key).is_none());
    }

    #[test]
    fn bech32m_variant_uses_its_own_constant() {
        let key = [0x77u8; 32];
        let b32 = encode_variant("npub", &key, Variant::Bech32).unwrap();
        let b32m = encode_variant("npub", &key, Variant::Bech32m).unwrap();
        assert_ne!(b32, b32m);
        // Payloads match; only the six checksum characters differ.
        assert_eq!(b32[..b32.len() - 6], b32m[..b32m.len() - 6]);
        assert!(decode_raw(&b32m, Variant::Bech32m).is_some());
        assert!(decode_raw(&b32m, Variant::Bech32).is_none());
        assert!(decode(&b32m).is_none());
    }

    #[test]
    fn convert_bits_round_trips_and_enforces_padding() {
        let bytes: Vec<u8> = (0..40u8).collect();
        let five = convert_bits(&bytes, 8, 5, true).unwrap();
        assert_eq!(five.len(), (bytes.len() * 8).div_ceil(5));
        assert_eq!(
            convert_bits(&five, 5, 8, false).as_deref(),
            Some(&bytes[..])
        );
        // 40 bytes is exactly 64 five-bit groups; one extra group leaves five
        // leftover bits, which is a whole source group and so illegal.
        let mut too_long = five.clone();
        too_long.push(0);
        assert!(convert_bits(&too_long, 5, 8, false).is_none());
        // Values that do not fit the source group width are rejected.
        assert!(convert_bits(&[32u8], 5, 8, true).is_none());
    }

    #[test]
    fn separator_placement_rules() {
        let key = [0x09u8; 32];
        let encoded = npub(&key).unwrap();
        // Strip the separator: the last '1' then lands inside what would be
        // the data part, which is not a charset character, so this fails.
        let no_sep = encoded.replacen('1', "", 1);
        assert!(decode(&no_sep).is_none());
        // Separator too close to the end (fewer than six data characters).
        assert!(decode("npub1qqqqq").is_none());
        // Trailing separator: empty data part.
        assert!(decode(&format!("{encoded}1")).is_none());
    }
}
