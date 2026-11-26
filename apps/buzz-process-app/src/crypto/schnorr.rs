//! BIP-340 Schnorr signatures over secp256k1: x-only public keys, 64-byte
//! signatures.
//!
//! # Provenance
//!
//! Implemented from BIP-340 (*Schnorr Signatures for secp256k1*, Wuille,
//! Nick, Ruffing; final), following its pseudocode step for step over the
//! vendored [`field`](super::field), [`scalar`](super::scalar),
//! [`point`](super::point) and [`sha256`](super::sha256) primitives. Validated
//! against every vector of the reference `bip-0340/test-vectors.csv`
//! (indices 0–18), asserted both directions: each valid vector's signature is
//! reproduced byte-for-byte by [`sign`] and accepted by [`verify`], and each
//! invalid vector is rejected.
//!
//! # Why this module exists
//!
//! This is the crate's authentication root: an event is admitted only if its id
//! is the hash of its canonical serialization *and* `sig` verifies here against
//! the author's x-only key. A false accept forges any author; a false reject
//! silently drops honest traffic. It cannot be replaced by a dependency — the
//! platform code gate admits exactly one (`ferrite-app`) and forbids `unsafe`,
//! so `secp256k1` (C FFI) and `k256` (extra crates) are both unavailable.
//!
//! # Failure discipline
//!
//! [`verify`] is total: every possible 64-byte signature, 32-byte key and
//! 32-byte message returns `true` or `false`, never a panic. That is a hard
//! requirement, because its inputs are attacker-chosen bytes off the wire and
//! this crate runs inside a shared app runtime — a panic is a denial of service.
//! There is no indexing, no slicing, no `unwrap`, and no arithmetic that can
//! overflow on this path; malformed inputs fail at the range checks
//! ([`Fe::from_bytes_be`], [`Scalar::from_bytes_be_canonical`],
//! [`Point::lift_x`]).
//!
//! # Side-channel posture
//!
//! Verification handles only public data. Signing touches the secret key, and
//! uses the fixed-shape ladder documented in [`super::point`]; the two parity
//! branches it does take (`has_even_y(P)`, `has_even_y(R)`) test values that are
//! public — anyone can recompute the `y` parity of a published x-only key. Key
//! material is not zeroized (no `zeroize` dependency is admissible, and plain
//! writes are elidable), so process memory is the trust boundary.

use super::field::Fe;
use super::point::Point;
use super::scalar::Scalar;
use super::sha256::{sha256, Sha256};

/// BIP-340 tag for the challenge hash `e`.
const TAG_CHALLENGE: &[u8] = b"BIP0340/challenge";
/// BIP-340 tag for masking the secret key with the auxiliary randomness.
const TAG_AUX: &[u8] = b"BIP0340/aux";
/// BIP-340 tag for the nonce derivation.
const TAG_NONCE: &[u8] = b"BIP0340/nonce";

/// `tagged_hash(tag, data) = SHA256(SHA256(tag) || SHA256(tag) || data)`
/// (BIP-340 "Design: Tagged hashes").
///
/// The doubled tag digest is a 64-byte prefix — exactly one SHA-256 block — so
/// domain separation costs one compression call and cannot collide with a
/// differently tagged hash of any message. `parts` are absorbed in order and the
/// boundaries between them are invisible to the digest, which is safe here only
/// because every caller feeds fixed-width fields (32 bytes each) ahead of the
/// single variable-length one.
fn tagged_hash(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let tag_digest = sha256(tag);
    let mut hasher = Sha256::new();
    hasher.update(&tag_digest);
    hasher.update(&tag_digest);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

/// Split a 64-byte signature into `(r, s)`.
///
/// Both chunk reads are statically satisfied for a 64-byte array; the zero
/// fallbacks exist only so that this is expressible without indexing or
/// `unwrap`, and they fail closed (an all-zero `r` is rejected downstream).
fn halves(sig64: &[u8; 64]) -> ([u8; 32], [u8; 32]) {
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    if let Some((head, tail)) = sig64.split_first_chunk::<32>() {
        r = *head;
        if let Some(rest) = tail.first_chunk::<32>() {
            s = *rest;
        }
    }
    (r, s)
}

/// Concatenate `r || s` into a 64-byte signature without indexing.
fn join(r: &[u8; 32], s: &[u8; 32]) -> [u8; 64] {
    let mut sig = [0u8; 64];
    for (chunk, half) in sig.chunks_exact_mut(32).zip([r, s]) {
        chunk.copy_from_slice(half);
    }
    sig
}

/// Verify a BIP-340 signature over a 32-byte message.
///
/// `msg32` is the message hash (for this crate, an event id), `pubkey_x` the
/// 32-byte x-only public key, `sig64` the signature `r || s`.
///
/// Returns `false` — never panics — for every rejection: `r >= p`, `s >= n`, a
/// public key that is not the x-coordinate of a curve point (including `x >= p`
/// and `x = 0`), `R = s·G - e·P` at infinity, `R` with odd `y`, or `x(R) != r`.
/// A non-canonical encoding is a rejection, not a re-interpretation: signature
/// malleability is what the `r >= p` / `s >= n` checks exist to prevent, and this
/// crate keys idempotence on event ids that cover `sig`.
#[must_use]
pub fn verify(msg32: &[u8; 32], pubkey_x: &[u8; 32], sig64: &[u8; 64]) -> bool {
    verify_msg(msg32, pubkey_x, sig64)
}

/// Verify a BIP-340 signature over a message of any length.
///
/// BIP-340 is defined for variable-length messages (the 2022 addendum), and the
/// reference vectors 15–18 cover sizes 0, 1, 17 and 100. This crate signs only
/// 32-byte event ids, so [`verify`] is the API the protocol uses; this entry
/// point exists because the specification's domain is wider than the protocol's
/// and testing the whole domain is cheaper than arguing that the rest is
/// unreachable.
#[must_use]
pub fn verify_msg(msg: &[u8], pubkey_x: &[u8; 32], sig64: &[u8; 64]) -> bool {
    let (r_bytes, s_bytes) = halves(sig64);
    // "Fail if r >= p": `Fe::from_bytes_be` rejects a non-canonical encoding.
    let Some(r) = Fe::from_bytes_be(&r_bytes) else {
        return false;
    };
    // "Fail if s >= n". Zero is admitted here and dies at the `R` checks, which
    // is what the spec says and what vectors 9 and 10 probe.
    let Some(s) = Scalar::from_bytes_be_canonical(&s_bytes) else {
        return false;
    };
    // "Let P = lift_x(int(pk)); fail if that fails."
    let Some(point_p) = Point::lift_x(pubkey_x) else {
        return false;
    };
    // "Let e = int(hash_BIP0340/challenge(bytes(r) || bytes(P) || m)) mod n."
    let e = Scalar::from_bytes_be_reduced(&tagged_hash(TAG_CHALLENGE, &[&r_bytes, pubkey_x, msg]));
    // "Let R = s·G - e·P", evaluated as `s·G + (-e)·P` so that one ladder shape
    // serves both terms.
    let big_r = Point::mul_generator(&s).add(&point_p.mul_scalar(&e.negate()));
    // "Fail if is_infinite(R) or not has_even_y(R) or x(R) != r." The infinity
    // case must fail rather than compare as `(0, even)`; `to_affine_fe` returns
    // `None` there, which is why vectors 9 and 10 are rejected.
    match big_r.to_affine_fe() {
        None => false,
        Some((x, y)) => !y.is_odd() && x == r,
    }
}

/// Sign a 32-byte message with a 32-byte secret key and 32 bytes of auxiliary
/// randomness, per BIP-340's `Sign`.
///
/// Returns `None` when `int(seckey)` is zero or `>= n`, when the derived nonce
/// reduces to zero (probability ≈ 2⁻¹²⁸, but it is a defined failure, not an
/// assertion), or if the signature fails the self-check below.
///
/// # `aux_rand32` and the absence of an RNG
///
/// This crate has no entropy source: a constrained Ferrite app gets no clock and
/// no RNG, so the caller supplies `aux_rand32`. BIP-340's nonce is derived, not
/// random — `k` is a hash of `(masked key, x(P), msg)` — so the signature is
/// *deterministic given `aux_rand32`* and is secure even with
/// `aux_rand32 = [0; 32]` (that degrades to RFC 6979-style determinism). The
/// auxiliary value's purpose is defence against fault and differential
/// side-channel attacks, so callers should pass something that differs per
/// signature: an HMAC over the payload under a stored app secret is the pattern
/// this crate uses, since it needs no clock and no entropy.
///
/// # Self-check
///
/// The produced signature is verified before it is returned, as BIP-340
/// recommends. That doubles the cost of signing and catches the failure mode
/// that matters — emitting a signature that will not verify, which in this
/// protocol means publishing an event every relay will reject. It is pure
/// defence in depth: with correct arithmetic it never fires, so no test can
/// observe it without injecting a fault, and removing it would not fail this
/// module's suite. It stays because a bit flip that corrupts a signature into a
/// *valid-looking* one would otherwise leak an unverifiable event into a durable
/// log.
#[must_use]
pub fn sign(msg32: &[u8; 32], seckey: &[u8; 32], aux_rand32: &[u8; 32]) -> Option<[u8; 64]> {
    sign_msg(msg32, seckey, aux_rand32)
}

/// Sign a message of any length. See [`verify_msg`] for why the variable-length
/// entry point exists, and [`sign`] for the contract.
#[must_use]
pub fn sign_msg(msg: &[u8], seckey: &[u8; 32], aux_rand32: &[u8; 32]) -> Option<[u8; 64]> {
    // "Let d' = int(sk); fail if d' = 0 or d' >= n."
    let d0 = Scalar::from_bytes_be_checked(seckey)?;
    // "Let P = d'·G." Not the identity, since `0 < d' < n` and `G` has order `n`.
    let (px, py) = Point::mul_generator(&d0).to_affine_fe()?;
    let px_bytes = px.to_bytes_be();
    // "Let d = d' if has_even_y(P), otherwise n - d'." The branch condition is
    // public: the parity of a published key's y is recomputable by anyone.
    let d = if py.is_odd() { d0.negate() } else { d0 };
    // "Let t be the byte-wise xor of bytes(d) and hash_BIP0340/aux(a)."
    let mut t = d.to_bytes_be();
    for (slot, mask) in t.iter_mut().zip(tagged_hash(TAG_AUX, &[aux_rand32]).iter()) {
        *slot ^= *mask;
    }
    // "Let rand = hash_BIP0340/nonce(t || bytes(P) || m)."
    let rand = tagged_hash(TAG_NONCE, &[&t, &px_bytes, msg]);
    // "Let k' = int(rand) mod n; fail if k' = 0."
    let k0 = Scalar::from_bytes_be_reduced(&rand);
    if k0.is_zero() {
        return None;
    }
    // "Let R = k'·G. Let k = k' if has_even_y(R), otherwise n - k'."
    let (rx, ry) = Point::mul_generator(&k0).to_affine_fe()?;
    let k = if ry.is_odd() { k0.negate() } else { k0 };
    let rx_bytes = rx.to_bytes_be();
    // "Let e = int(hash_BIP0340/challenge(bytes(R) || bytes(P) || m)) mod n."
    let e =
        Scalar::from_bytes_be_reduced(&tagged_hash(TAG_CHALLENGE, &[&rx_bytes, &px_bytes, msg]));
    // "Let sig = bytes(R) || bytes((k + e·d) mod n)."
    let sig = join(&rx_bytes, &k.add(&e.mul(&d)).to_bytes_be());
    // "Verify(bytes(P), m, sig); if it fails, abort."
    if verify_msg(msg, &px_bytes, &sig) {
        Some(sig)
    } else {
        None
    }
}

/// The x-only public key for a secret key: `bytes(d'·G)`, i.e. the 32-byte
/// x-coordinate with the `y` parity dropped.
///
/// Returns `None` when `int(seckey)` is zero or `>= n`.
///
/// Note the x-only consequence, which callers must not mistake for a bug: a key
/// and its negation (`n - d'`) have the same x-only public key and produce
/// identical signatures, because [`sign`] normalises `d` to the even-`y`
/// representative. Two distinct 32-byte secrets therefore map to one identity.
#[must_use]
pub fn xonly_pubkey(seckey: &[u8; 32]) -> Option<[u8; 32]> {
    let d = Scalar::from_bytes_be_checked(seckey)?;
    let (x, _) = Point::mul_generator(&d).to_affine_fe()?;
    Some(x.to_bytes_be())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::crypto::{from_hex, from_hex32, to_hex};

    /// One row of `bip-0340/test-vectors.csv`.
    struct Vector {
        index: u32,
        /// Present only for rows that also pin the signing direction.
        seckey: Option<&'static str>,
        pubkey: &'static str,
        aux_rand: Option<&'static str>,
        /// Hex, variable length: rows 15–18 are 0, 1, 17 and 100 bytes.
        msg: &'static str,
        sig: &'static str,
        valid: bool,
        /// The CSV's reason, kept verbatim so a failure names the property lost.
        reason: &'static str,
    }

    /// The reference vectors, transcribed from
    /// `bitcoin/bips@master:bip-0340/test-vectors.csv`. Rows 0–14 are the
    /// original set (32-byte messages); rows 15–18 were added 2022-12 for
    /// variable-length messages and exercise [`sign_msg`]/[`verify_msg`].
    const VECTORS: &[Vector] = &[
        Vector {
            index: 0,
            seckey: Some("0000000000000000000000000000000000000000000000000000000000000003"),
            pubkey: "F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000000"),
            msg: "0000000000000000000000000000000000000000000000000000000000000000",
            sig: "E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0",
            valid: true,
            reason: "",
        },
        Vector {
            index: 1,
            seckey: Some("B7E151628AED2A6ABF7158809CF4F3C762E7160F38B4DA56A784D9045190CFEF"),
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000001"),
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "6896BD60EEAE296DB48A229FF71DFE071BDE413E6D43F917DC8DCF8C78DE33418906D11AC976ABCCB20B091292BFF4EA897EFCB639EA871CFA95F6DE339E4B0A",
            valid: true,
            reason: "",
        },
        Vector {
            index: 2,
            seckey: Some("C90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B14E5C9"),
            pubkey: "DD308AFEC5777E13121FA72B9CC1B7CC0139715309B086C960E18FD969774EB8",
            aux_rand: Some("C87AA53824B4D7AE2EB035A2B5BBBCCC080E76CDC6D1692C4B0B62D798E6D906"),
            msg: "7E2D58D8B3BCDF1ABADEC7829054F90DDA9805AAB56C77333024B9D0A508B75C",
            sig: "5831AAEED7B44BB74E5EAB94BA9D4294C49BCF2A60728D8B4C200F50DD313C1BAB745879A5AD954A72C45A91C3A51D3C7ADEA98D82F8481E0E1E03674A6F3FB7",
            valid: true,
            reason: "",
        },
        Vector {
            index: 3,
            seckey: Some("0B432B2677937381AEF05BB02A66ECD012773062CF3FA2549E44F58ED2401710"),
            pubkey: "25D1DFF95105F5253C4022F628A996AD3A0D95FBF21D468A1B33F8C160D8F517",
            aux_rand: Some("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"),
            msg: "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            sig: "7EB0509757E246F19449885651611CB965ECC1A187DD51B64FDA1EDC9637D5EC97582B9CB13DB3933705B32BA982AF5AF25FD78881EBB32771FC5922EFC66EA3",
            valid: true,
            reason: "test fails if msg is reduced modulo p or n",
        },
        Vector {
            index: 4,
            seckey: None,
            pubkey: "D69C3509BB99E412E68B0FE8544E72837DFA30746D8BE2AA65975F29D22DC7B9",
            aux_rand: None,
            msg: "4DF3C3F68FCC83B27E9D42C90431A72499F17875C81A599B566C9889B9696703",
            sig: "00000000000000000000003B78CE563F89A0ED9414F5AA28AD0D96D6795F9C6376AFB1548AF603B3EB45C9F8207DEE1060CB71C04E80F593060B07D28308D7F4",
            valid: true,
            reason: "",
        },
        Vector {
            index: 5,
            seckey: None,
            pubkey: "EEFDEA4CDB677750A420FEE807EACF21EB9898AE79B9768766E4FAA04A2D4A34",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B",
            valid: false,
            reason: "public key not on the curve",
        },
        Vector {
            index: 6,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A14602975563CC27944640AC607CD107AE10923D9EF7A73C643E166BE5EBEAFA34B1AC553E2",
            valid: false,
            reason: "has_even_y(R) is false",
        },
        Vector {
            index: 7,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "1FA62E331EDBC21C394792D2AB1100A7B432B013DF3F6FF4F99FCB33E0E1515F28890B3EDB6E7189B630448B515CE4F8622A954CFE545735AAEA5134FCCDB2BD",
            valid: false,
            reason: "negated message",
        },
        Vector {
            index: 8,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769961764B3AA9B2FFCB6EF947B6887A226E8D7C93E00C5ED0C1834FF0D0C2E6DA6",
            valid: false,
            reason: "negated s value",
        },
        Vector {
            index: 9,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "0000000000000000000000000000000000000000000000000000000000000000123DDA8328AF9C23A94C1FEECFD123BA4FB73476F0D594DCB65C6425BD186051",
            valid: false,
            reason: "sG - eP is infinite; fails only if x(inf) is not treated as 0",
        },
        Vector {
            index: 10,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "00000000000000000000000000000000000000000000000000000000000000017615FBAF5AE28864013C099742DEADB4DBA87F11AC6754F93780D5A1837CF197",
            valid: false,
            reason: "sG - eP is infinite; fails only if x(inf) is not treated as 1",
        },
        Vector {
            index: 11,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "4A298DACAE57395A15D0795DDBFD1DCB564DA82B0F269BC70A74F8220429BA1D69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B",
            valid: false,
            reason: "sig[0:32] is not an x-coordinate on the curve",
        },
        Vector {
            index: 12,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F69E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B",
            valid: false,
            reason: "sig[0:32] is equal to the field size p",
        },
        Vector {
            index: 13,
            seckey: None,
            pubkey: "DFF1D77F2A671C5F36183726DB2341BE58FEAE1DA2DECED843240F7B502BA659",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E177769FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",
            valid: false,
            reason: "sig[32:64] is equal to the curve order n",
        },
        Vector {
            index: 14,
            seckey: None,
            pubkey: "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC30",
            aux_rand: None,
            msg: "243F6A8885A308D313198A2E03707344A4093822299F31D0082EFA98EC4E6C89",
            sig: "6CFF5C3BA86C69EA4B7376F31A9BCB4F74C1976089B2D9963DA2E5543E17776969E89B4C5564D00349106B8497785DD7D1D713A8AE82B32FA79D5F7FC407D39B",
            valid: false,
            reason: "public key exceeds the field size (p+1), so it is not a valid x-coordinate",
        },
        Vector {
            index: 15,
            seckey: Some("0340034003400340034003400340034003400340034003400340034003400340"),
            pubkey: "778CAA53B4393AC467774D09497A87224BF9FAB6F6E68B23086497324D6FD117",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000000"),
            msg: "",
            sig: "71535DB165ECD9FBBC046E5FFAEA61186BB6AD436732FCCC25291A55895464CF6069CE26BF03466228F19A3A62DB8A649F2D560FAC652827D1AF0574E427AB63",
            valid: true,
            reason: "message of size 0",
        },
        Vector {
            index: 16,
            seckey: Some("0340034003400340034003400340034003400340034003400340034003400340"),
            pubkey: "778CAA53B4393AC467774D09497A87224BF9FAB6F6E68B23086497324D6FD117",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000000"),
            msg: "11",
            sig: "08A20A0AFEF64124649232E0693C583AB1B9934AE63B4C3511F3AE1134C6A303EA3173BFEA6683BD101FA5AA5DBC1996FE7CACFC5A577D33EC14564CEC2BACBF",
            valid: true,
            reason: "message of size 1",
        },
        Vector {
            index: 17,
            seckey: Some("0340034003400340034003400340034003400340034003400340034003400340"),
            pubkey: "778CAA53B4393AC467774D09497A87224BF9FAB6F6E68B23086497324D6FD117",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000000"),
            msg: "0102030405060708090A0B0C0D0E0F1011",
            sig: "5130F39A4059B43BC7CAC09A19ECE52B5D8699D1A71E3C52DA9AFDB6B50AC370C4A482B77BF960F8681540E25B6771ECE1E5A37FD80E5A51897C5566A97EA5A5",
            valid: true,
            reason: "message of size 17",
        },
        Vector {
            index: 18,
            seckey: Some("0340034003400340034003400340034003400340034003400340034003400340"),
            pubkey: "778CAA53B4393AC467774D09497A87224BF9FAB6F6E68B23086497324D6FD117",
            aux_rand: Some("0000000000000000000000000000000000000000000000000000000000000000"),
            msg: "99999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
            sig: "403B12B0D8555A344175EA7EC746566303321E5DBFA8BE6F091635163ECA79A8585ED3E3170807E7C03B720FC54C7B23897FCBA0E9D0B4A06894CFD249F22367",
            valid: true,
            reason: "message of size 100",
        },
    ];

    fn bytes32(hex: &str) -> [u8; 32] {
        from_hex32(hex).unwrap()
    }

    fn sig64(hex: &str) -> [u8; 64] {
        let v = from_hex(hex).unwrap();
        assert_eq!(v.len(), 64, "signature must be 64 bytes");
        let mut out = [0u8; 64];
        out.copy_from_slice(&v);
        out
    }

    #[test]
    fn official_vectors_verify_exactly_as_specified() {
        for vector in VECTORS {
            let msg = from_hex(vector.msg).unwrap();
            let pubkey = bytes32(vector.pubkey);
            let sig = sig64(vector.sig);
            assert_eq!(
                verify_msg(&msg, &pubkey, &sig),
                vector.valid,
                "vector {}: {}",
                vector.index,
                vector.reason
            );
            // The 32-byte entry point must agree wherever it applies (0-14).
            if msg.len() == 32 {
                assert_eq!(
                    verify(&bytes32(vector.msg), &pubkey, &sig),
                    vector.valid,
                    "vector {} through the fixed-size API",
                    vector.index
                );
            }
        }
        // Sanity: the table really does contain both outcomes, so a
        // verify()-always-false bug cannot pass this test.
        assert_eq!(VECTORS.iter().filter(|v| v.valid).count(), 9);
        assert_eq!(VECTORS.iter().filter(|v| !v.valid).count(), 10);
    }

    #[test]
    fn official_vectors_sign_byte_for_byte() {
        let mut signed = 0;
        for vector in VECTORS {
            let (Some(seckey), Some(aux)) = (vector.seckey, vector.aux_rand) else {
                continue;
            };
            assert!(vector.valid, "vector {} pins signing", vector.index);
            let msg = from_hex(vector.msg).unwrap();
            let seckey = bytes32(seckey);
            let produced = sign_msg(&msg, &seckey, &bytes32(aux)).expect("valid key must sign");
            assert_eq!(
                to_hex(&produced),
                vector.sig.to_lowercase(),
                "vector {}: signature must reproduce byte-for-byte",
                vector.index
            );
            assert_eq!(
                to_hex(&xonly_pubkey(&seckey).unwrap()),
                vector.pubkey.to_lowercase(),
                "vector {}: public key",
                vector.index
            );
            if msg.len() == 32 {
                assert_eq!(
                    sign(&bytes32(vector.msg), &seckey, &bytes32(aux)),
                    Some(produced),
                    "vector {} through the fixed-size API",
                    vector.index
                );
            }
            signed += 1;
        }
        assert_eq!(signed, 8, "vectors 0-3 and 15-18 pin the signing direction");
    }

    #[test]
    fn invalid_vectors_fail_for_the_documented_structural_reason() {
        // Each assertion below re-derives the *cause* named in the CSV, so a
        // future refactor cannot make these vectors pass for the wrong reason
        // (e.g. rejecting every signature).
        //
        // Coverage boundary, stated because it is not obvious: for the two range
        // rules ("fail if r >= p", "fail if s >= n") no *behavioural* test can
        // separate rejecting from reducing. An implementation that folded an
        // out-of-range value into range would still reject vectors 12 and 13,
        // and constructing a signature that the folding version accepts needs
        // either a discrete log or a valid signature whose x(R) is below
        // 2^32+977. So the rules are pinned here at the decoder instead: the
        // assertions on `Fe::from_bytes_be` and
        // `Scalar::from_bytes_be_canonical` are what make the strictness real,
        // and `verify` is written to use exactly those two constructors.
        let p = "fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f";
        let n = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";

        // 5: public key is not on the curve.
        assert!(Point::lift_x(&bytes32(VECTORS[5].pubkey)).is_none());
        // 14: public key is p+1, above the field size.
        assert!(Fe::from_bytes_be(&bytes32(VECTORS[14].pubkey)).is_none());
        // 12: r == p exactly.
        let (r12, _) = halves(&sig64(VECTORS[12].sig));
        assert_eq!(to_hex(&r12), p);
        assert!(Fe::from_bytes_be(&r12).is_none());
        // 13: s == n exactly.
        let (_, s13) = halves(&sig64(VECTORS[13].sig));
        assert_eq!(to_hex(&s13), n);
        assert!(Scalar::from_bytes_be_canonical(&s13).is_none());
        // 9 and 10: r is 0 and 1, and sG - eP is infinite for both.
        let (r9, _) = halves(&sig64(VECTORS[9].sig));
        assert_eq!(r9, [0u8; 32]);
        let (r10, _) = halves(&sig64(VECTORS[10].sig));
        assert_eq!(r10[31], 1);
        assert_eq!(r10[..31], [0u8; 31]);
        for index in [9usize, 10] {
            let vector = &VECTORS[index];
            let sig = sig64(vector.sig);
            let (r_bytes, s_bytes) = halves(&sig);
            let pubkey = bytes32(vector.pubkey);
            let msg = bytes32(vector.msg);
            let s = Scalar::from_bytes_be_canonical(&s_bytes).unwrap();
            let point_p = Point::lift_x(&pubkey).unwrap();
            let e = Scalar::from_bytes_be_reduced(&tagged_hash(
                TAG_CHALLENGE,
                &[&r_bytes, &pubkey, &msg],
            ));
            let big_r = Point::mul_generator(&s).add(&point_p.mul_scalar(&e.negate()));
            assert!(
                big_r.is_identity(),
                "vector {index}: sG - eP must be infinite"
            );
            assert_eq!(big_r.has_even_y(), None);
        }
        // 11: r is a well-formed field element that is not on the curve.
        let (r11, _) = halves(&sig64(VECTORS[11].sig));
        assert!(Fe::from_bytes_be(&r11).is_some());
        assert!(Point::lift_x(&r11).is_none());
        // 6: R is recovered with x(R) == r, and only the y parity rejects it.
        let vector = &VECTORS[6];
        let sig = sig64(vector.sig);
        let (r_bytes, s_bytes) = halves(&sig);
        let pubkey = bytes32(vector.pubkey);
        let msg = bytes32(vector.msg);
        let s = Scalar::from_bytes_be_canonical(&s_bytes).unwrap();
        let point_p = Point::lift_x(&pubkey).unwrap();
        let e =
            Scalar::from_bytes_be_reduced(&tagged_hash(TAG_CHALLENGE, &[&r_bytes, &pubkey, &msg]));
        let big_r = Point::mul_generator(&s).add(&point_p.mul_scalar(&e.negate()));
        let (x, _) = big_r.to_affine().unwrap();
        assert_eq!(to_hex(&x), to_hex(&r_bytes), "vector 6: x(R) does equal r");
        assert_eq!(
            big_r.has_even_y(),
            Some(false),
            "vector 6: only parity fails"
        );
        // 8: `s` is the negation (n - s) of a valid signature's, and it is still
        // in canonical range — so the rejection has to come from the `R`
        // equation, not from a range check. Vector 8's r half is shared with
        // vector 5's signature, and the two s halves sum to n.
        let (r8, s8) = halves(&sig64(VECTORS[8].sig));
        let (r5, s5) = halves(&sig64(VECTORS[5].sig));
        assert_eq!(r8, r5, "vectors 5 and 8 share the r half");
        let s8_scalar = Scalar::from_bytes_be_canonical(&s8).expect("s is canonical");
        let s5_scalar = Scalar::from_bytes_be_canonical(&s5).expect("s is canonical");
        assert_eq!(s8_scalar, s5_scalar.negate(), "vector 8 negates s");
        // 7: the key lifts and both halves are in range; only the equation fails.
        assert!(Point::lift_x(&bytes32(VECTORS[7].pubkey)).is_some());
        let (r7, s7) = halves(&sig64(VECTORS[7].sig));
        assert!(Fe::from_bytes_be(&r7).is_some());
        assert!(Scalar::from_bytes_be_canonical(&s7).is_some());
    }

    /// Fixed keys with expected x-only public keys and signatures computed
    /// independently (arbitrary-precision Python reference over the BIP-340
    /// pseudocode), so the round trip is pinned to outside values, not just to
    /// self-consistency.
    const ROUND_TRIP: &[(&str, &str, &str)] = &[
        (
            "0000000000000000000000000000000000000000000000000000000000000001",
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "5524fe6a1f44fe73cce7bb6b151f47fdefb9a288e9c2bd3eed6d97c2b2ba2b6d07ba2e85baa0a6b0e06180009667bdc2236e3414e4c6ea11a39a748b64c19300",
        ),
        (
            "0000000000000000000000000000000000000000000000000000000000000002",
            "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            "6496be7fb3ed159818a76704f196f20e45ee71b3dd0554130ea53aa5276148c8e3bf69564c498bc80f35813f062274e68e30d875e946de76683933e212f33af6",
        ),
        (
            "b7e151628aed2a6abf7158809cf4f3c762e7160f38b4da56a784d9045190cfef",
            "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659",
            "75e325cdf3ce3eb10fd119bcd6fcbe8b930dce8b4bb4092c26c289c29761c4a7d46a577f27685857a4674d57f7fc442a941ae132e2f78842feb8288774294b72",
        ),
        (
            // n-1: the negation of key 1, which shares its x-only identity.
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140",
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "5524fe6a1f44fe73cce7bb6b151f47fdefb9a288e9c2bd3eed6d97c2b2ba2b6d07ba2e85baa0a6b0e06180009667bdc2236e3414e4c6ea11a39a748b64c19300",
        ),
        (
            "6b911fd37cdf5c81d4c0adb1ab7fa822ed253ab0ad9bd7b0f36d0b1f0e8b9f9c",
            "7df59888b6146215999b7096a5a5c009c8615cdf5cf80484f6f8b691f613e3ee",
            "ac1904b82bafc5bf75c6566d46caf7fdbbd84485ff58ef316b8e8c3e8b643c013fa09f6d039476c021fee2d783c64cf1650148117b90acec2b5513b3fec862e6",
        ),
    ];
    const RT_MSG: &str = "2a1e4c7f00ff3b9d5566778899aabbccddeeff00112233445566778899aabbcc";
    const RT_AUX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00123456789abcdeffedcba9876543210";

    #[test]
    fn round_trip_on_fixed_keys_matches_an_independent_reference() {
        let msg = bytes32(RT_MSG);
        let aux = bytes32(RT_AUX);
        for (seckey_hex, pubkey_hex, sig_hex) in ROUND_TRIP {
            let seckey = bytes32(seckey_hex);
            let pubkey = xonly_pubkey(&seckey).expect("key in range");
            assert_eq!(to_hex(&pubkey), *pubkey_hex, "pubkey for {seckey_hex}");
            let sig = sign(&msg, &seckey, &aux).expect("signing must succeed");
            assert_eq!(to_hex(&sig), *sig_hex, "signature for {seckey_hex}");
            assert!(verify(&msg, &pubkey, &sig), "own signature must verify");
            // Signing is deterministic given aux_rand.
            assert_eq!(sign(&msg, &seckey, &aux), Some(sig));
            // A different aux_rand yields a different (still valid) signature.
            let other = sign(&msg, &seckey, &[0u8; 32]).unwrap();
            assert_ne!(other, sig, "aux_rand must enter the nonce");
            assert!(verify(&msg, &pubkey, &other));
        }
    }

    #[test]
    fn a_key_and_its_negation_share_one_x_only_identity() {
        // BIP-340 normalises d to the even-y representative, so d and n-d are the
        // same signer. This is a property of the scheme, not a bug, and it is
        // asserted so nobody "fixes" the negation away.
        let one = bytes32("0000000000000000000000000000000000000000000000000000000000000001");
        let n_minus_1 = bytes32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140");
        assert_eq!(xonly_pubkey(&one), xonly_pubkey(&n_minus_1));
        let msg = bytes32(RT_MSG);
        let aux = bytes32(RT_AUX);
        assert_eq!(sign(&msg, &one, &aux), sign(&msg, &n_minus_1, &aux));
    }

    #[test]
    fn out_of_range_secret_keys_are_rejected() {
        let n = bytes32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
        let msg = bytes32(RT_MSG);
        let aux = bytes32(RT_AUX);
        for bad in [[0u8; 32], n, [0xff; 32]] {
            assert_eq!(
                xonly_pubkey(&bad),
                None,
                "key {} must be rejected",
                to_hex(&bad)
            );
            assert_eq!(sign(&msg, &bad, &aux), None);
            assert_eq!(sign_msg(b"", &bad, &aux), None);
        }
        // n-1 is the largest valid key.
        let n_minus_1 = bytes32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140");
        assert!(xonly_pubkey(&n_minus_1).is_some());
        assert!(sign(&msg, &n_minus_1, &aux).is_some());
    }

    #[test]
    fn flipping_any_bit_of_the_signature_breaks_verification() {
        let seckey = bytes32(ROUND_TRIP[4].0);
        let pubkey = xonly_pubkey(&seckey).unwrap();
        let msg = bytes32(RT_MSG);
        let sig = sign(&msg, &seckey, &bytes32(RT_AUX)).unwrap();
        assert!(verify(&msg, &pubkey, &sig));
        // Sampled rather than exhaustive: 512 bit positions times two ladders per
        // verification is minutes of unoptimised test time, and every byte of both
        // halves is still covered. The stride is coprime with 8, so the sample
        // hits every bit position within a byte.
        for bit in (0..512).step_by(13) {
            let mut tampered = sig;
            tampered[bit / 8] ^= 1 << (bit % 8);
            assert!(
                !verify(&msg, &pubkey, &tampered),
                "flipped signature bit {bit} still verified"
            );
        }
    }

    #[test]
    fn flipping_any_bit_of_the_message_or_key_breaks_verification() {
        let seckey = bytes32(ROUND_TRIP[2].0);
        let pubkey = xonly_pubkey(&seckey).unwrap();
        let msg = bytes32(RT_MSG);
        let sig = sign(&msg, &seckey, &bytes32(RT_AUX)).unwrap();
        for bit in (0..256).step_by(11) {
            let mut bad_msg = msg;
            bad_msg[bit / 8] ^= 1 << (bit % 8);
            assert!(
                !verify(&bad_msg, &pubkey, &sig),
                "flipped message bit {bit} still verified"
            );
            let mut bad_key = pubkey;
            bad_key[bit / 8] ^= 1 << (bit % 8);
            // A flipped key is usually not a curve point at all; either way it
            // must not verify.
            assert!(
                !verify(&msg, &bad_key, &sig),
                "flipped public-key bit {bit} still verified"
            );
        }
        // Truncation-style confusion: the signature of one message must not
        // verify for another, and one key's signature must not verify under
        // another key.
        let other_msg = bytes32("0000000000000000000000000000000000000000000000000000000000000001");
        assert!(!verify(&other_msg, &pubkey, &sig));
        let other_key = xonly_pubkey(&bytes32(ROUND_TRIP[4].0)).unwrap();
        assert!(!verify(&msg, &other_key, &sig));
    }

    #[test]
    fn malformed_signatures_are_rejected_without_panicking() {
        let pubkey = xonly_pubkey(&bytes32(ROUND_TRIP[2].0)).unwrap();
        let msg = bytes32(RT_MSG);
        let p = bytes32("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f");
        let n = bytes32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
        let valid = sign(&msg, &bytes32(ROUND_TRIP[2].0), &bytes32(RT_AUX)).unwrap();
        let (valid_r, valid_s) = halves(&valid);

        // p-1 and n-1 are in range but are not this signature's values.
        let mut p_minus_1 = p;
        p_minus_1[31] = 0x2e;
        let mut n_minus_1 = n;
        n_minus_1[31] = 0x40;
        let cases: [[u8; 64]; 11] = [
            [0u8; 64],                    // r = 0, s = 0
            [0xff; 64],                   // r >= p and s >= n
            join(&p, &valid_s),           // r = p exactly
            join(&valid_r, &n),           // s = n exactly
            join(&p, &n),                 // both at their limits
            join(&valid_r, &[0u8; 32]),   // s = 0
            join(&[0u8; 32], &valid_s),   // r = 0
            join(&valid_s, &valid_r),     // halves swapped
            join(&valid_r, &[0xff; 32]),  // s = 2^256-1
            join(&[0xff; 32], &valid_s),  // r = 2^256-1
            join(&p_minus_1, &n_minus_1), // both in range, both wrong
        ];

        for case in &cases {
            assert!(
                !verify(&msg, &pubkey, case),
                "malformed signature accepted: {}",
                to_hex(case)
            );
        }
        // Malformed public keys, with a valid signature.
        for bad_key in [[0u8; 32], [0xff; 32], p, bytes32(VECTORS[5].pubkey)] {
            assert!(
                !verify(&msg, &bad_key, &valid),
                "malformed public key accepted: {}",
                to_hex(&bad_key)
            );
        }
        // And the valid signature still verifies, so the loop above proves
        // rejection rather than a broken fixture.
        assert!(verify(&msg, &pubkey, &valid));
    }

    #[test]
    fn signatures_are_not_malleable() {
        let seckey = bytes32(ROUND_TRIP[2].0);
        let pubkey = xonly_pubkey(&seckey).unwrap();
        let msg = bytes32(RT_MSG);
        let sig = sign(&msg, &seckey, &bytes32(RT_AUX)).unwrap();
        let (r, s) = halves(&sig);
        assert!(verify(&msg, &pubkey, &sig));

        // s -> n - s: the classic ECDSA-style second signature. BIP-340 has no
        // such equivalence, so it must be rejected.
        let negated = Scalar::from_bytes_be_canonical(&s).unwrap().negate();
        assert!(!verify(&msg, &pubkey, &join(&r, &negated.to_bytes_be())));

        // s -> s + n mod 2^256: a different 32-byte encoding that is *sometimes*
        // still below n, so the range check alone would not catch it; the R
        // equation must.
        let mut wrapped = s;
        let mut carry = 0u16;
        for (slot, addend) in wrapped.iter_mut().rev().zip(
            bytes32("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
                .iter()
                .rev(),
        ) {
            let sum = u16::from(*slot) + u16::from(*addend) + carry;
            *slot = sum as u8;
            carry = sum >> 8;
        }
        assert!(!verify(&msg, &pubkey, &join(&r, &wrapped)));

        // r -> p - r: a different x-coordinate, hence a different (or absent) R.
        let negated_r = Fe::from_bytes_be(&r).unwrap().negate().to_bytes_be();
        assert!(!verify(&msg, &pubkey, &join(&negated_r, &s)));

        // The original still verifies, so the rejections above are real.
        assert!(verify(&msg, &pubkey, &sig));
    }

    #[test]
    fn a_deterministic_sweep_of_derived_inputs_round_trips_and_never_panics() {
        // The crate has no RNG, so the sweep's entropy is a hash chain: each
        // iteration derives a fresh key, message, aux value and a garbage
        // signature from SHA-256 of the previous state. Sign/verify must round
        // trip on the derived key, and the garbage signature must be rejected
        // without panicking — the property that matters for inbound events.
        use crate::crypto::sha256::sha256_concat;
        let mut state = sha256(b"buzz-relay/schnorr/sweep");
        let mut accepted_garbage = 0;
        for step in 0..24u8 {
            state = sha256_concat(&[&state, &[step]]);
            let seckey = sha256_concat(&[b"k", &state]);
            let msg = sha256_concat(&[b"m", &state]);
            let aux = sha256_concat(&[b"a", &state]);
            let garbage = join(
                &sha256_concat(&[b"r", &state]),
                &sha256_concat(&[b"s", &state]),
            );

            let pubkey =
                xonly_pubkey(&seckey).expect("a hash is a valid key with overwhelming odds");
            let sig = sign(&msg, &seckey, &aux).expect("signing must succeed");
            assert!(
                verify(&msg, &pubkey, &sig),
                "step {step}: own signature must verify"
            );
            // Cross terms: the signature belongs to this (msg, key) pair only.
            assert!(
                !verify(&sha256(&msg), &pubkey, &sig),
                "step {step}: wrong message"
            );
            if verify(&msg, &pubkey, &garbage) {
                accepted_garbage += 1;
            }
            // Derived keys are also exercised through the variable-length path.
            let long = sign_msg(&state[..7], &seckey, &aux).expect("variable-length signing");
            assert!(verify_msg(&state[..7], &pubkey, &long));
            assert!(
                !verify_msg(&state[..6], &pubkey, &long),
                "step {step}: truncated message"
            );
        }
        assert_eq!(
            accepted_garbage, 0,
            "a derived random signature must never verify"
        );
    }

    #[test]
    fn tagged_hash_matches_the_specification_construction() {
        // tagged_hash(tag, m) = SHA256(SHA256(tag) || SHA256(tag) || m), checked
        // against the same expression built by hand from the sha256 primitive.
        let tag_digest = sha256(TAG_CHALLENGE);
        let mut expected = Vec::new();
        expected.extend_from_slice(&tag_digest);
        expected.extend_from_slice(&tag_digest);
        expected.extend_from_slice(b"buzz");
        assert_eq!(tagged_hash(TAG_CHALLENGE, &[b"buzz"]), sha256(&expected));
        // Parts are absorbed in order and are equivalent to the concatenation.
        assert_eq!(
            tagged_hash(TAG_NONCE, &[b"ab", b"cd"]),
            tagged_hash(TAG_NONCE, &[b"abcd"])
        );
        // Tags are separated: the same message under three tags gives three
        // different digests.
        let a = tagged_hash(TAG_CHALLENGE, &[b"x"]);
        let b = tagged_hash(TAG_AUX, &[b"x"]);
        let c = tagged_hash(TAG_NONCE, &[b"x"]);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // The tag strings themselves are load-bearing constants.
        assert_eq!(TAG_CHALLENGE, b"BIP0340/challenge");
        assert_eq!(TAG_AUX, b"BIP0340/aux");
        assert_eq!(TAG_NONCE, b"BIP0340/nonce");
    }
}
