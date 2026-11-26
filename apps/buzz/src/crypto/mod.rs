//! The buzz protocol's cryptography.
//!
//! The curve work is vendored, implemented in safe Rust from published
//! specifications, because the platform code gate admits exactly one dependency
//! (`ferrite-app`) and forbids `unsafe`/FFI — so `secp256k1` and `bech32`
//! cannot be linked. Each submodule carries its own provenance header.
//!
//! The digest and MAC are not vendored. `ferrite_app::crypto` ships
//! SHA-256, HMAC-SHA-256, the constant-time comparator and hex, so [`sha256`]
//! and [`hmac`] are the `packages/sha256-hmac` shape over the SDK's kernels
//! rather than second implementations of them.
//!
//! Scope is deliberately the minimum the buzz protocol needs:
//!
//! * [`sha256`] — event ids, digests, key derivation.
//! * [`hmac`] — stream tokens, webhook secrets, cursor signing.
//! * [`schnorr`] — BIP-340 verify (every inbound event) and sign (relay- and
//!   agent-authored events).
//! * [`bech32`] — `npub`/`nsec` display encoding.
//!
//! Constant-time discipline: secret-dependent branches are avoided in
//! [`schnorr`]; [`ct_eq`] is the only comparison used on secrets.

pub mod bech32;
pub mod field;
pub mod point;
pub mod scalar;
pub mod schnorr;

// The digest and the MAC are `packages/sha256-hmac`, vendored at
// `src/crypto_kernel/` where the package owns its whole tree including its
// root module. They are re-exported under their long-standing names here so
// every `crate::crypto::sha256::…` call site keeps resolving: a package may
// not be mounted ON TOP of a module the app also writes, because then one
// side would silently overwrite the other on the next sync.
pub use crate::crypto_kernel::{hmac, sha256};

/// Constant-time byte equality. Returns `true` only if both slices have the
/// same length and identical contents; the work does not depend on where the
/// first difference is.
///
/// The comparator is `crate::crypto_kernel::constant_time_eq`; this is the name
/// the relay and the curve code already call it by.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    crate::crypto_kernel::constant_time_eq(a, b)
}

/// Lowercase hex encoding, from `crate::crypto_kernel::hex_encode`.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    crate::crypto_kernel::hex_encode(bytes)
}

/// Decode lowercase or uppercase hex. Returns `None` on odd length or a
/// non-hex byte. From `crate::crypto_kernel::hex_decode`.
#[must_use]
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    crate::crypto_kernel::hex_decode(s)
}

/// Decode exactly 32 bytes of hex.
#[must_use]
pub fn from_hex32(s: &str) -> Option<[u8; 32]> {
    let v = from_hex(s)?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_only_identical_slices() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00u8, 0x0f, 0xff, 0xa5];
        let hex = to_hex(&bytes);
        assert_eq!(hex, "000fffa5");
        assert_eq!(from_hex(&hex).as_deref(), Some(&bytes[..]));
        assert_eq!(from_hex("0F").as_deref(), Some(&[0x0fu8][..]));
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
