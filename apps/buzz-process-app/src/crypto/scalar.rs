//! Arithmetic modulo `n`, the order of the secp256k1 group:
//!
//! ```text
//! n = fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141
//! ```
//!
//! # Provenance
//!
//! `n` is the group order fixed by SEC 2 v2 §2.4.1. Reduction of a wide product
//! is plain binary long division (see [`reduce_wide_mod_n`]); inversion is
//! Fermat exponentiation by `n-2`. Every expected value in the tests was
//! computed independently in arbitrary-precision arithmetic.
//!
//! # Representation
//!
//! [`Scalar`] is four little-endian 64-bit limbs, always canonical (`< n`). The
//! canonical invariant is why [`Scalar::to_bytes_be`] is the unique encoding a
//! BIP-340 signature carries, and why derived `PartialEq` is correct.
//!
//! # Why the arithmetic here is allowed to be slow
//!
//! BIP-340 *verification* — the hot path, run on every inbound event — needs no
//! multiplication in this ring at all: the challenge is a 256-bit value reduced
//! once ([`Scalar::from_bytes_be_reduced`]), and everything else happens on the
//! curve. *Signing* needs one multiplication and one addition. So this module
//! optimises for being auditable, not for throughput, and it uses masked
//! selection instead of branches on secret magnitudes.

use crate::crypto::field::{add4, mul_wide, select4, sub4};

/// The group order `n`, little-endian limbs.
const N: [u64; 4] = [
    0xbfd2_5e8c_d036_4141,
    0xbaae_dce6_af48_a03b,
    0xffff_ffff_ffff_fffe,
    0xffff_ffff_ffff_ffff,
];

/// `n - 2`, the inversion exponent (Fermat: `a^(n-2) = a^-1` for prime `n`).
const N_MINUS_2: [u64; 4] = [
    0xbfd2_5e8c_d036_413f,
    0xbaae_dce6_af48_a03b,
    0xffff_ffff_ffff_fffe,
    0xffff_ffff_ffff_ffff,
];

/// An integer modulo the group order, canonically reduced to `[0, n)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scalar([u64; 4]);

impl Scalar {
    /// The additive identity.
    pub const ZERO: Self = Self([0, 0, 0, 0]);

    /// The multiplicative identity.
    pub const ONE: Self = Self([1, 0, 0, 0]);

    /// A small scalar. `v < 2^64 < n`, so the result is canonical.
    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        Self([v, 0, 0, 0])
    }

    /// The additive identity, as a function (mirrors [`Scalar::ZERO`]).
    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    /// The multiplicative identity, as a function (mirrors [`Scalar::ONE`]).
    #[must_use]
    pub const fn one() -> Self {
        Self::ONE
    }

    /// Decode a 32-byte big-endian integer, rejecting both a non-canonical
    /// encoding (`>= n`) and zero.
    ///
    /// This is the secret-key rule: BIP-340 signing fails if `d' = 0` or
    /// `d' >= n`, and a zero scalar would put the public key at infinity.
    #[must_use]
    pub fn from_bytes_be_checked(bytes: &[u8; 32]) -> Option<Self> {
        let s = Self::from_bytes_be_canonical(bytes)?;
        if s.is_zero() {
            None
        } else {
            Some(s)
        }
    }

    /// Decode a 32-byte big-endian integer, rejecting only a non-canonical
    /// encoding (`>= n`); zero is accepted.
    ///
    /// This is exactly the rule BIP-340 verification applies to `sig[32:64]`:
    /// "fail if `s >= n`" — and no more than that, so a signature with `s = 0`
    /// stays representable and is rejected by the `R` checks rather than by the
    /// decode.
    #[must_use]
    pub fn from_bytes_be_canonical(bytes: &[u8; 32]) -> Option<Self> {
        let limbs = limbs_from_bytes_be(bytes);
        let (_, borrow) = sub4(&limbs, &N);
        if borrow == 0 {
            None // limbs >= n
        } else {
            Some(Self(limbs))
        }
    }

    /// Decode a 32-byte big-endian integer, reducing modulo `n`.
    ///
    /// This is the `int(hash) mod n` of the BIP-340 challenge and nonce, and the
    /// only reduction on the verification path. One conditional subtraction is
    /// sufficient because `n > 2^255`, so any 256-bit input is `< 2n`.
    #[must_use]
    pub fn from_bytes_be_reduced(bytes: &[u8; 32]) -> Self {
        let limbs = limbs_from_bytes_be(bytes);
        let (reduced, borrow) = sub4(&limbs, &N);
        Self(select4(&limbs, &reduced, borrow == 0))
    }

    /// The unique 32-byte big-endian encoding of this scalar.
    #[must_use]
    pub fn to_bytes_be(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (chunk, limb) in out.chunks_exact_mut(8).zip(self.0.iter().rev()) {
            chunk.copy_from_slice(&limb.to_be_bytes());
        }
        out
    }

    /// Whether this is the zero scalar.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        (self.0.iter().fold(0u64, |acc, l| acc | *l)) == 0
    }

    /// Bit `index` of the canonical representative, counting from the least
    /// significant. Indices `>= 256` read as `false`, which lets a ladder walk a
    /// fixed number of iterations without bounds bookkeeping.
    #[must_use]
    pub fn bit(&self, index: usize) -> bool {
        bit_of(&self.0, index)
    }

    /// Modular addition.
    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        let (sum, carry) = add4(&self.0, &other.0);
        let (reduced, borrow) = sub4(&sum, &N);
        // Both inputs are `< n`, so the true sum is `< 2n < 2^257`: one
        // conditional subtraction canonicalises. When the sum overflowed 2^256
        // the wrapped difference `sum - n + 2^256` is the answer, and it fits
        // because a carry forces `sum < n`.
        let take = (carry | (borrow ^ 1)) == 1;
        Self(select4(&sum, &reduced, take))
    }

    /// Modular subtraction.
    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        let (diff, borrow) = sub4(&self.0, &other.0);
        // On borrow the wrapped difference is `a - b + 2^256`; adding `n` and
        // dropping the (certain) carry yields `a - b + n`, in `[0, n)`.
        let (fixed, _) = add4(&diff, &N);
        Self(select4(&diff, &fixed, borrow == 1))
    }

    /// The additive inverse: `n - self`, and zero for zero.
    ///
    /// BIP-340 uses this twice: to flip a secret key whose public point has odd
    /// `y`, and to negate the challenge so `s·G - e·P` can be evaluated as
    /// `s·G + (-e)·P`.
    #[must_use]
    pub fn negate(&self) -> Self {
        let (neg, _) = sub4(&N, &self.0);
        Self(select4(&neg, &Self::ZERO.0, self.is_zero()))
    }

    /// Modular multiplication.
    #[must_use]
    pub fn mul(&self, other: &Self) -> Self {
        Self(reduce_wide_mod_n(&mul_wide(&self.0, &other.0)))
    }

    /// The multiplicative inverse, or `None` for zero.
    ///
    /// `self^(n-2)` by square-and-multiply over the fixed public exponent
    /// `n - 2`, 256 iterations regardless of input. Nothing in BIP-340 needs
    /// this; it exists because a scalar ring without inversion is a trap for
    /// later callers (threshold signing, blinding), and it is pinned against
    /// independently computed inverses in the tests.
    #[must_use]
    pub fn invert(&self) -> Option<Self> {
        if self.is_zero() {
            return None;
        }
        let mut acc = Self::ONE;
        for index in (0..256).rev() {
            acc = acc.mul(&acc);
            if bit_of(&N_MINUS_2, index) {
                acc = acc.mul(self);
            }
        }
        Some(acc)
    }
}

/// Decode 32 big-endian bytes into little-endian limbs, without range checks.
fn limbs_from_bytes_be(bytes: &[u8; 32]) -> [u64; 4] {
    let mut limbs = [0u64; 4];
    // `chunks_exact(8)` yields the four groups most-significant first; `rev()`
    // pairs them with limb 3 down to limb 0.
    for (slot, chunk) in limbs.iter_mut().rev().zip(bytes.chunks_exact(8)) {
        let mut v = 0u64;
        for b in chunk {
            v = (v << 8) | u64::from(*b);
        }
        *slot = v;
    }
    limbs
}

/// Bit `index` (from the least significant) of a little-endian limb array;
/// `false` past the end.
#[inline]
fn bit_of(limbs: &[u64], index: usize) -> bool {
    limbs
        .get(index / 64)
        .is_some_and(|limb| (limb >> (index % 64)) & 1 == 1)
}

/// Reduce a 512-bit little-endian value modulo `n`, canonically.
///
/// Binary long division: walk the 512 bits from the top keeping a running
/// remainder `rem < n`; each step doubles it, shifts the next bit in, and
/// conditionally subtracts `n`. One subtraction per bit is always enough because
/// `2·rem + 1 <= 2n - 1 < 2n`. The subtraction is masked, so the loop's work and
/// control flow are identical for every input — the wide products reaching here
/// are products of secrets (`e·d` in signing).
///
/// Folding with `2^256 - n` would be roughly an order of magnitude faster, at the
/// cost of a multi-width carry argument to audit. Nothing on the verification
/// path reaches this function, so the trade goes to clarity.
fn reduce_wide_mod_n(w: &[u64; 8]) -> [u64; 4] {
    let mut rem = [0u64; 4];
    for index in (0..512).rev() {
        // rem = 2·rem + bit, remembering the bit shifted out of the top limb.
        let mut carry = u64::from(bit_of(w, index));
        for limb in rem.iter_mut() {
            let shifted_out = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = shifted_out;
        }
        let (reduced, borrow) = sub4(&rem, &N);
        rem = select4(&rem, &reduced, (carry | (borrow ^ 1)) == 1);
    }
    rem
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::crypto::from_hex32;

    fn sc(hex: &str) -> Scalar {
        Scalar::from_bytes_be_canonical(&from_hex32(hex).unwrap()).unwrap()
    }

    const N_HEX: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";
    const N_MINUS_1_HEX: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140";
    const SA_HEX: &str = "1e2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
    const SC_HEX: &str = "0000000000000000000000000000000100000000000000000000000000000001";

    #[test]
    fn checked_decoding_rejects_the_order_and_zero() {
        let n = from_hex32(N_HEX).unwrap();
        assert!(
            Scalar::from_bytes_be_checked(&n).is_none(),
            "n must be rejected"
        );
        assert!(
            Scalar::from_bytes_be_canonical(&n).is_none(),
            "n must be rejected"
        );
        assert!(Scalar::from_bytes_be_checked(&[0xff; 32]).is_none());
        assert!(
            Scalar::from_bytes_be_checked(&[0u8; 32]).is_none(),
            "zero is not a key"
        );
        // Canonical admits zero; checked does not. That difference is exactly
        // what BIP-340 needs between `s` and `d'`.
        assert_eq!(
            Scalar::from_bytes_be_canonical(&[0u8; 32]),
            Some(Scalar::ZERO)
        );
        assert!(Scalar::from_bytes_be_checked(&from_hex32(N_MINUS_1_HEX).unwrap()).is_some());
    }

    #[test]
    fn reduced_decoding_folds_once_at_the_order() {
        assert_eq!(
            Scalar::from_bytes_be_reduced(&from_hex32(N_HEX).unwrap()),
            Scalar::ZERO,
            "n reduces to 0"
        );
        let mut n_plus_1 = from_hex32(N_HEX).unwrap();
        n_plus_1[31] = 0x42;
        assert_eq!(Scalar::from_bytes_be_reduced(&n_plus_1), Scalar::ONE);
        assert_eq!(
            Scalar::from_bytes_be_reduced(&[0xff; 32]),
            sc("000000000000000000000000000000014551231950b75fc4402da1732fc9bebe")
        );
        // Below the order, reduction is the identity.
        assert_eq!(
            Scalar::from_bytes_be_reduced(&from_hex32(SA_HEX).unwrap()),
            sc(SA_HEX)
        );
        assert_eq!(
            Scalar::from_bytes_be_reduced(
                &from_hex32("8000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap()
            ),
            sc("8000000000000000000000000000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn bytes_round_trip_canonically() {
        for hex in [SA_HEX, SC_HEX, N_MINUS_1_HEX] {
            assert_eq!(sc(hex).to_bytes_be(), from_hex32(hex).unwrap(), "{hex}");
        }
        assert_eq!(Scalar::ZERO.to_bytes_be(), [0u8; 32]);
    }

    #[test]
    fn add_and_sub_wrap_at_the_order_edge() {
        let n_minus_1 = sc(N_MINUS_1_HEX);
        assert_eq!(n_minus_1.add(&Scalar::ONE), Scalar::ZERO);
        assert_eq!(Scalar::ZERO.sub(&Scalar::ONE), n_minus_1);
        // (n-1) + (n-1) = n-2
        assert_eq!(n_minus_1.add(&n_minus_1), n_minus_1.sub(&Scalar::ONE));
        // Independent expected value.
        assert_eq!(
            sc(SA_HEX).add(&n_minus_1),
            sc("1e2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2a")
        );
        assert_eq!(sc(SA_HEX).sub(&sc(SA_HEX)), Scalar::ZERO);
    }

    #[test]
    fn negate_is_the_additive_inverse() {
        assert_eq!(Scalar::ZERO.negate(), Scalar::ZERO);
        for hex in [SA_HEX, SC_HEX, N_MINUS_1_HEX] {
            let x = sc(hex);
            assert_eq!(x.add(&x.negate()), Scalar::ZERO, "{hex}");
            assert_eq!(x.negate().negate(), x, "{hex}");
        }
        assert_eq!(Scalar::ONE.negate(), sc(N_MINUS_1_HEX));
    }

    #[test]
    fn mul_matches_independent_known_answers() {
        let a = sc(SA_HEX);
        let b = sc(N_MINUS_1_HEX);
        let c = sc(SC_HEX);
        assert_eq!(
            a.mul(&b),
            sc("e1d4c3b2a190857463f2e1d0c5b4a3913c1f42db931b61ec6566e1ff32272716")
        );
        assert_eq!(
            a.mul(&c),
            sc("c31149b7acf5b734e5421924fe4a32d7eb10f9c8593bc8d0f788bc289f76f57e")
        );
        // (n-1)^2 = 1
        assert_eq!(b.mul(&b), Scalar::ONE);
        assert_eq!(
            c.mul(&c),
            sc("000000000000000000000000000000034551231950b75fc4402da1732fc9bec0")
        );
        assert_eq!(a.mul(&Scalar::ONE), a);
        assert_eq!(a.mul(&Scalar::ZERO), Scalar::ZERO);
        assert_eq!(a.mul(&c), c.mul(&a));
        // a·2 = a + a checks the wide reduction against the cheap path.
        assert_eq!(a.mul(&Scalar::from_u64(2)), a.add(&a));
        assert_eq!(b.mul(&Scalar::from_u64(2)), b.add(&b));
    }

    #[test]
    fn invert_times_value_is_one() {
        assert!(Scalar::ZERO.invert().is_none());
        assert_eq!(Scalar::ONE.invert().unwrap(), Scalar::ONE);
        for hex in [SA_HEX, SC_HEX, N_MINUS_1_HEX] {
            let x = sc(hex);
            assert_eq!(x.mul(&x.invert().unwrap()), Scalar::ONE, "{hex}");
        }
        for small in [2u64, 3, 7, u64::MAX] {
            let x = Scalar::from_u64(small);
            assert_eq!(x.mul(&x.invert().unwrap()), Scalar::ONE, "{small}");
        }
        // Independent expected values (Python `pow(x, -1, n)`).
        assert_eq!(
            sc(SA_HEX).invert().unwrap(),
            sc("0f5c87e31c5c7d586a21f300961081e1db9d05229a108cdc879587cca46c7914")
        );
        assert_eq!(
            Scalar::from_u64(2).invert().unwrap(),
            sc("7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a1")
        );
    }

    #[test]
    fn bit_reads_the_little_endian_bit_order() {
        assert!(Scalar::ONE.bit(0));
        assert!(!Scalar::ONE.bit(1));
        assert!(!Scalar::ONE.bit(255));
        assert!(!Scalar::ONE.bit(256), "out of range reads false");
        assert!(!Scalar::ONE.bit(usize::MAX / 64), "no panic past the end");
        let two_255 = sc("8000000000000000000000000000000000000000000000000000000000000000");
        assert!(two_255.bit(255));
        assert!(!two_255.bit(254));
        // n-1 is even, and its top bits are set.
        let n_minus_1 = sc(N_MINUS_1_HEX);
        assert!(!n_minus_1.bit(0));
        assert!(n_minus_1.bit(6));
        assert!(n_minus_1.bit(255));
    }

    #[test]
    fn ring_axioms_hold_on_a_deterministic_walk() {
        // No RNG exists in this crate, so a deterministic iteration supplies the
        // spread of inputs: x -> x^2 + 7 crosses limb boundaries quickly.
        let mut x = sc(SA_HEX);
        let mut y = sc(SC_HEX);
        for _ in 0..24 {
            x = x.mul(&x).add(&Scalar::from_u64(7));
            y = y.mul(&x).add(&Scalar::ONE);
            assert_eq!(x.mul(&y.add(&Scalar::ONE)), x.mul(&y).add(&x));
            assert_eq!(x.add(&y).mul(&x.sub(&y)), x.mul(&x).sub(&y.mul(&y)));
            assert_eq!(x.negate().mul(&y), x.mul(&y).negate());
        }
    }
}
