//! Arithmetic in GF(p), the secp256k1 base field, with
//! `p = 2^256 - 2^32 - 977`.
//!
//! # Provenance
//!
//! The modulus is the one fixed by SEC 2 v2 §2.4.1 (secp256k1). Reduction is the
//! textbook pseudo-Mersenne fold: writing `p = 2^256 - C` with
//! `C = 2^32 + 977`, any 512-bit value `H·2^256 + L` is congruent to `L + H·C`,
//! so a wide product collapses in two folds and one conditional subtraction. The
//! inversion and square-root addition chains are the chains libsecp256k1
//! publishes in `field_impl.h`; both are re-derived in the doc comments of
//! [`Fe::inverse`] and [`Fe::sqrt`] and pinned by tests against values computed
//! independently in arbitrary-precision arithmetic.
//!
//! # Representation
//!
//! [`Fe`] is four little-endian 64-bit limbs —
//! `v = l0 + l1·2^64 + l2·2^128 + l3·2^192` — and is **always canonical**: every
//! constructor and every operation returns a representative in `[0, p)`. That
//! invariant is load-bearing twice over: it makes derived `PartialEq` a correct
//! field comparison, and it makes [`Fe::to_bytes_be`] the unique 32-byte
//! encoding that BIP-340 compares byte-for-byte.
//!
//! # What this module deliberately does not do
//!
//! There is no `unsafe`, no assembly, no wide division, and no table. Carries
//! ride `u128` intermediates, which cannot overflow: a 64×64 product plus two
//! 64-bit addends is at most `2^128 - 1`. Reductions use masked selection rather
//! than branches, so magnitude comparisons on secret intermediates do not turn
//! into data-dependent jumps; the module makes no stronger constant-time claim
//! than that (see [`crate::relay::crypto::schnorr`] for the threat model).

/// The field modulus `p = 2^256 - 2^32 - 977`, little-endian limbs.
const P: [u64; 4] = [
    0xffff_fffe_ffff_fc2f,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
];

/// `C = 2^256 - p = 2^32 + 977`. Held as `u128` because every use of it is a
/// `u128` product with a 64-bit limb.
const C: u128 = 0x1_0000_03d1;

/// An element of GF(p), canonically reduced to `[0, p)`.
///
/// Equality is limb equality, which is exact because the canonical invariant
/// holds for every value this module can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fe([u64; 4]);

impl Fe {
    /// The additive identity.
    pub const ZERO: Self = Self([0, 0, 0, 0]);

    /// The multiplicative identity.
    pub const ONE: Self = Self([1, 0, 0, 0]);

    /// A small field element. `v < 2^64 < p`, so the result is canonical.
    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        Self([v, 0, 0, 0])
    }

    /// Build from raw little-endian limbs.
    ///
    /// # Contract
    ///
    /// The caller guarantees `limbs < p`. This exists only for compile-time
    /// curve constants (there is no `const` byte decoder because rejection has
    /// to be expressible), and every such constant is checked against its
    /// standard big-endian encoding in tests.
    pub(crate) const fn from_limbs(limbs: [u64; 4]) -> Self {
        Self(limbs)
    }

    /// Decode a 32-byte big-endian integer, rejecting any value `>= p`.
    ///
    /// Rejection (rather than reduction) is what BIP-340 requires of the `r`
    /// component of a signature and of an x-only public key: a non-canonical
    /// encoding must not be accepted as a different valid value.
    #[must_use]
    pub fn from_bytes_be(bytes: &[u8; 32]) -> Option<Self> {
        let mut limbs = [0u64; 4];
        // `chunks_exact(8)` walks the four 8-byte groups most-significant first;
        // `rev()` on the limbs pairs that with limb 3 down to limb 0.
        for (slot, chunk) in limbs.iter_mut().rev().zip(bytes.chunks_exact(8)) {
            let mut v = 0u64;
            for b in chunk {
                v = (v << 8) | u64::from(*b);
            }
            *slot = v;
        }
        let (_, borrow) = sub4(&limbs, &P);
        if borrow == 0 {
            None // limbs >= p
        } else {
            Some(Self(limbs))
        }
    }

    /// The unique 32-byte big-endian encoding of this element.
    #[must_use]
    pub fn to_bytes_be(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (chunk, limb) in out.chunks_exact_mut(8).zip(self.0.iter().rev()) {
            chunk.copy_from_slice(&limb.to_be_bytes());
        }
        out
    }

    /// Whether this is the zero element.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        (self.0.iter().fold(0u64, |acc, l| acc | *l)) == 0
    }

    /// Whether the canonical representative is odd — the parity BIP-340 uses to
    /// pin down which of the two `y` values an x-only key means.
    #[must_use]
    pub fn is_odd(&self) -> bool {
        self.0.first().is_some_and(|l| l & 1 == 1)
    }

    /// Modular addition.
    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        let (sum, carry) = add4(&self.0, &other.0);
        let (reduced, borrow) = sub4(&sum, &P);
        // Both inputs are `< p`, so the true sum is `< 2p < 2^257` and exactly
        // one conditional subtraction canonicalises it. Take `sum - p` when the
        // 257-bit sum is at least `p`: either it overflowed 2^256 (`carry` set,
        // and then the wrapped difference `sum - p + 2^256` is the right answer,
        // which fits because `sum < 2p - 2^256 = p - C`), or the subtraction did
        // not borrow.
        let take = (carry | (borrow ^ 1)) == 1;
        Self(select4(&sum, &reduced, take))
    }

    /// Modular subtraction.
    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        let (diff, borrow) = sub4(&self.0, &other.0);
        // On borrow the wrapped difference is `a - b + 2^256`; adding p and
        // dropping the (certain) carry yields `a - b + p`, which is in `[0, p)`
        // because `|a - b| < p`.
        let (fixed, _) = add4(&diff, &P);
        Self(select4(&diff, &fixed, borrow == 1))
    }

    /// The additive inverse: `p - self`, and zero for zero.
    #[must_use]
    pub fn negate(&self) -> Self {
        let (neg, _) = sub4(&P, &self.0);
        // `p - 0 = p` is not canonical, so zero has to be selected explicitly.
        Self(select4(&neg, &Self::ZERO.0, self.is_zero()))
    }

    /// `self + self`.
    #[must_use]
    pub fn double(&self) -> Self {
        self.add(self)
    }

    /// `self * self * self`.
    #[must_use]
    pub fn cube(&self) -> Self {
        self.sqr().mul(self)
    }

    /// Modular multiplication.
    #[must_use]
    pub fn mul(&self, other: &Self) -> Self {
        Self(reduce_wide(&mul_wide(&self.0, &other.0)))
    }

    /// Modular squaring.
    ///
    /// This is `mul(self, self)`. A dedicated squaring kernel would save six of
    /// the sixteen 64×64 products, but it needs a second carry-propagation path
    /// for the doubled off-diagonal terms; on the hot path (verification) that
    /// buys a few percent, and it is not worth a second kernel to get wrong.
    #[must_use]
    pub fn sqr(&self) -> Self {
        self.mul(self)
    }

    /// Repeated squaring, `self^(2^times)` — the workhorse of the addition
    /// chains below.
    #[must_use]
    fn sqr_n(&self, times: u32) -> Self {
        let mut acc = *self;
        for _ in 0..times {
            acc = acc.sqr();
        }
        acc
    }

    /// Shared prefix of the inversion and square-root chains: returns
    /// `(self^(2^2-1), self^(2^22-1), self^(2^223-1))`.
    ///
    /// Each step multiplies a run of ones by a shifted run of ones, so the
    /// exponents are exactly the repunits named in the bindings.
    fn chain_prefix(&self) -> (Self, Self, Self) {
        let x1 = *self;
        let x2 = x1.sqr().mul(&x1); // 2^2-1
        let x3 = x2.sqr().mul(&x1); // 2^3-1
        let x6 = x3.sqr_n(3).mul(&x3);
        let x9 = x6.sqr_n(3).mul(&x3);
        let x11 = x9.sqr_n(2).mul(&x2);
        let x22 = x11.sqr_n(11).mul(&x11);
        let x44 = x22.sqr_n(22).mul(&x22);
        let x88 = x44.sqr_n(44).mul(&x44);
        let x176 = x88.sqr_n(88).mul(&x88);
        let x220 = x176.sqr_n(44).mul(&x44);
        let x223 = x220.sqr_n(3).mul(&x3);
        (x2, x22, x223)
    }

    /// The multiplicative inverse, or `None` for zero.
    ///
    /// Computed as `self^(p-2)` by Fermat's little theorem, over the
    /// libsecp256k1 addition chain. The exponent assembles as
    ///
    /// ```text
    /// p-2 = [223 ones][0][22 ones][00001][011][01]
    ///     = 0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2d
    /// ```
    ///
    /// which is where the trailing `sqr_n(23)/mul(x22)`, `sqr_n(5)/mul(self)`,
    /// `sqr_n(3)/mul(x2)`, `sqr_n(2)/mul(self)` steps come from: each `sqr_n`
    /// shifts in a block of zeros and each `mul` fills in the following run of
    /// ones (`x2 = 11`, `self = 1`).
    #[must_use]
    pub fn inverse(&self) -> Option<Self> {
        if self.is_zero() {
            return None;
        }
        let (x2, x22, x223) = self.chain_prefix();
        let t = x223.sqr_n(23).mul(&x22);
        let t = t.sqr_n(5).mul(self);
        let t = t.sqr_n(3).mul(&x2);
        Some(t.sqr_n(2).mul(self))
    }

    /// A square root of `self`, or `None` when `self` is not a quadratic
    /// residue.
    ///
    /// `p ≡ 3 (mod 4)`, so `self^((p+1)/4)` is a root whenever one exists. The
    /// exponent assembles as
    ///
    /// ```text
    /// (p+1)/4 = [223 ones][0][22 ones][000011][00]
    ///         = 0x3fffffffffffffffffffffffffffffffffffffffffffffffffffffffbfffff0c
    /// ```
    ///
    /// The candidate is always squared and compared before it is returned, which
    /// is the only thing that distinguishes a residue from a non-residue here —
    /// on a non-residue the exponentiation returns a value whose square is
    /// `-self`. Of the two roots this returns the one the chain lands on;
    /// callers that need a fixed parity (BIP-340 does) must negate as needed.
    #[must_use]
    pub fn sqrt(&self) -> Option<Self> {
        let (x2, x22, x223) = self.chain_prefix();
        let t = x223.sqr_n(23).mul(&x22);
        let t = t.sqr_n(6).mul(&x2);
        let root = t.sqr_n(2);
        if root.sqr() == *self {
            Some(root)
        } else {
            None
        }
    }

    /// Branch-free selection: `b` when `pick_b`, else `a`.
    ///
    /// Used by the scalar ladder in [`crate::relay::crypto::point`] so that the choice
    /// driven by a secret scalar bit is a mask, not a jump.
    #[must_use]
    pub fn select(a: &Self, b: &Self, pick_b: bool) -> Self {
        Self(select4(&a.0, &b.0, pick_b))
    }
}

// The three limb kernels below (and `mul_wide`) are modulus-agnostic
// multiprecision arithmetic, shared with [`crate::relay::crypto::scalar`] so that the
// carry, borrow and masking logic exists once and is tested once.

/// Add two 4-limb little-endian values; returns the wrapped sum and a carry of
/// 0 or 1.
#[inline]
pub(crate) fn add4(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], u64) {
    let mut out = [0u64; 4];
    let mut carry: u128 = 0;
    for (slot, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        let t = u128::from(*x) + u128::from(*y) + carry;
        *slot = t as u64;
        carry = t >> 64;
    }
    (out, carry as u64)
}

/// Subtract two 4-limb little-endian values; returns the wrapped difference
/// (`a - b mod 2^256`) and a borrow of 0 or 1. A zero borrow is exactly
/// `a >= b`, which is how every magnitude test in this module is phrased — it
/// costs the same for every input.
#[inline]
pub(crate) fn sub4(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], u64) {
    let mut out = [0u64; 4];
    let mut borrow: u128 = 0;
    for (slot, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        // `2^64 + x - y - borrow` never underflows a `u128`.
        let t = (u128::from(*x) | 1u128 << 64) - u128::from(*y) - borrow;
        *slot = t as u64;
        borrow = u128::from(t >> 64 == 0);
    }
    (out, borrow as u64)
}

/// Branch-free limb selection with an all-ones/all-zeros mask.
#[inline]
pub(crate) fn select4(a: &[u64; 4], b: &[u64; 4], pick_b: bool) -> [u64; 4] {
    let mask = 0u64.wrapping_sub(u64::from(pick_b));
    let mut out = [0u64; 4];
    for (slot, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *slot = (*x & !mask) | (*y & mask);
    }
    out
}

/// 4×4-limb schoolbook product, little-endian in and out.
///
/// Shared with [`crate::relay::crypto::scalar`], which needs the same 512-bit
/// intermediate before reducing modulo the group order; multiprecision
/// multiplication is modulus-agnostic, so there is one kernel, not two.
///
/// `#[allow(clippy::indexing_slicing)]`: `i` and `j` are bounded by the fixed
/// length 4 of the inputs, so `w[i + j]` (max index 6), `w[i + 4]` (max 7),
/// `a[i]` and `b[j]` are all in bounds by construction. Rewriting this with
/// iterators would obscure the carry chain, which is the one thing here that has
/// to be read closely.
#[allow(clippy::indexing_slicing)]
pub(crate) fn mul_wide(a: &[u64; 4], b: &[u64; 4]) -> [u64; 8] {
    let mut w = [0u64; 8];
    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in 0..4 {
            // Bound: (2^64-1)^2 + 2·(2^64-1) = 2^128 - 1, so no `u128` overflow.
            let t = u128::from(a[i]) * u128::from(b[j]) + u128::from(w[i + j]) + carry;
            w[i + j] = t as u64;
            carry = t >> 64;
        }
        // Untouched by every earlier row (row `i-1` wrote at most index `i+3`),
        // so this is an assignment, not an accumulation.
        w[i + 4] = carry as u64;
    }
    w
}

/// Reduce a 512-bit little-endian value modulo p, canonically.
fn reduce_wide(w: &[u64; 8]) -> [u64; 4] {
    let (lo, hi) = w.split_at(4);
    // Fold 1: `H·2^256 + L ≡ L + H·C`. `C < 2^33`, so each partial product is
    // `< 2^97` and the running carry stays `< 2^34`.
    let mut acc = [0u64; 4];
    let mut carry: u128 = 0;
    for (slot, (l, h)) in acc.iter_mut().zip(lo.iter().zip(hi.iter())) {
        let t = u128::from(*l) + u128::from(*h) * C + carry;
        *slot = t as u64;
        carry = t >> 64;
    }
    // Fold 2: `carry` is the weight-2^256 residue the first fold left behind, so
    // fold it in the same way. `carry·C < 2^67`.
    let mut t = carry * C;
    for slot in acc.iter_mut() {
        t += u128::from(*slot);
        *slot = t as u64;
        t >>= 64;
    }
    // Fold 3: fold 2 can overflow 2^256, but by at most one — and only when the
    // wrapped limbs are then `< 2^67`, since the overflowing sum was
    // `< 2^256 + 2^67`. So `t ∈ {0, 1}` and adding `t·C` cannot overflow again.
    let mut t = t * C;
    for slot in acc.iter_mut() {
        t += u128::from(*slot);
        *slot = t as u64;
        t >>= 64;
    }
    debug_assert_eq!(t, 0, "third fold must not overflow 2^256");
    // `acc < 2^256 = p + C`, so `acc - p < C`: one conditional subtraction is
    // enough to canonicalise.
    let (reduced, borrow) = sub4(&acc, &P);
    select4(&acc, &reduced, borrow == 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::crypto::from_hex32;

    /// Expected values in this module were computed independently in Python's
    /// arbitrary-precision integers (`pow(a, -1, p)`, `a*b % p`, and so on), not
    /// by this code.
    fn fe(hex: &str) -> Fe {
        Fe::from_bytes_be(&from_hex32(hex).unwrap()).unwrap()
    }

    const P_HEX: &str = "fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f";
    const P_MINUS_1_HEX: &str = "fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2e";
    const A_HEX: &str = "2a1e4c7f00ff3b9d5566778899aabbccddeeff00112233445566778899aabbcc";
    const CV_HEX: &str = "0123456789abcdeffedcba98765432100f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn from_bytes_be_rejects_the_modulus_and_above() {
        assert!(Fe::from_bytes_be(&from_hex32(P_HEX).unwrap()).is_none());
        assert!(Fe::from_bytes_be(&[0xff; 32]).is_none());
        // p+1 and p-1 straddle the boundary.
        let mut p_plus_1 = from_hex32(P_HEX).unwrap();
        p_plus_1[31] = 0x30;
        assert!(Fe::from_bytes_be(&p_plus_1).is_none());
        assert!(Fe::from_bytes_be(&from_hex32(P_MINUS_1_HEX).unwrap()).is_some());
        assert!(Fe::from_bytes_be(&[0u8; 32]).is_some());
    }

    #[test]
    fn bytes_round_trip_canonically() {
        for hex in [A_HEX, CV_HEX, P_MINUS_1_HEX] {
            let x = fe(hex);
            assert_eq!(x.to_bytes_be(), from_hex32(hex).unwrap(), "{hex}");
        }
        assert_eq!(Fe::ZERO.to_bytes_be(), [0u8; 32]);
        assert_eq!(Fe::from_u64(1), Fe::ONE);
        assert_eq!(
            Fe::from_u64(0x0102_0304_0506_0708).to_bytes_be()[24..],
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn add_and_sub_wrap_at_the_modulus_edge() {
        let p_minus_1 = fe(P_MINUS_1_HEX);
        assert_eq!(p_minus_1.add(&Fe::ONE), Fe::ZERO);
        assert_eq!(Fe::ZERO.sub(&Fe::ONE), p_minus_1);
        // (p-1) + (p-1) = p-2
        let p_minus_2 = p_minus_1.sub(&Fe::ONE);
        assert_eq!(p_minus_1.add(&p_minus_1), p_minus_2);
        assert_eq!(p_minus_1.double(), p_minus_2);
        // Known-answer sums/differences (Python).
        let a = fe(A_HEX);
        let b = p_minus_1;
        assert_eq!(
            a.add(&b),
            fe("2a1e4c7f00ff3b9d5566778899aabbccddeeff00112233445566778899aabbcb")
        );
        assert_eq!(
            a.sub(&b),
            fe("2a1e4c7f00ff3b9d5566778899aabbccddeeff00112233445566778899aabbcd")
        );
        assert_eq!(
            b.sub(&a),
            fe("d5e1b380ff00c462aa99887766554433221100ffeeddccbbaa99887666554062")
        );
        assert_eq!(a.sub(&a), Fe::ZERO);
    }

    #[test]
    fn negate_is_the_additive_inverse() {
        assert_eq!(Fe::ZERO.negate(), Fe::ZERO);
        for hex in [A_HEX, CV_HEX, P_MINUS_1_HEX] {
            let x = fe(hex);
            assert_eq!(x.add(&x.negate()), Fe::ZERO, "{hex}");
            assert_eq!(x.negate().negate(), x, "{hex}");
        }
        assert_eq!(Fe::ONE.negate(), fe(P_MINUS_1_HEX));
    }

    #[test]
    fn mul_matches_independent_known_answers() {
        let a = fe(A_HEX);
        let b = fe(P_MINUS_1_HEX);
        let c = fe(CV_HEX);
        assert_eq!(
            a.mul(&b),
            fe("d5e1b380ff00c462aa99887766554433221100ffeeddccbbaa99887666554063")
        );
        assert_eq!(
            a.mul(&c),
            fe("e747e4fd3b85a3d6593f32942ff7487b78fdf5241f85c53fbe8dc7e74e61e43a")
        );
        // (p-1)^2 = 1
        assert_eq!(b.mul(&b), Fe::ONE);
        assert_eq!(b.sqr(), Fe::ONE);
        assert_eq!(
            c.sqr(),
            fe("583b12598bf99dd16e06d2e75aff9c80529dfc5898ad7fc548b1ed06c0c2380f")
        );
        assert_eq!(
            a.sqr(),
            fe("a44a60ad0295e2cb9e6265d7be2ad6d38b2282b1d2e3e4a479e3d98289ba06d5")
        );
        // Commutativity, identity, annihilation, and the sqr/mul agreement that
        // matters if a dedicated squaring kernel is ever introduced.
        assert_eq!(a.mul(&c), c.mul(&a));
        assert_eq!(a.mul(&Fe::ONE), a);
        assert_eq!(a.mul(&Fe::ZERO), Fe::ZERO);
        assert_eq!(a.sqr(), a.mul(&a));
        assert_eq!(a.cube(), a.mul(&a).mul(&a));
    }

    #[test]
    fn mul_carries_propagate_across_all_limbs() {
        // (2^256 - 1) mod p folds twice; squaring p-1 and multiplying maximal
        // values is where a missed carry shows up.
        let max = fe(P_MINUS_1_HEX);
        assert_eq!(max.mul(&Fe::from_u64(2)), max.add(&max));
        // 2^255 * 2 = 2^256 = C mod p
        let two_255 = fe("8000000000000000000000000000000000000000000000000000000000000000");
        assert_eq!(two_255.double(), Fe::from_u64(0x1_0000_03d1));
        // 2^128 squared = 2^256 = C mod p
        let two_128 = fe("0000000000000000000000000000000100000000000000000000000000000000");
        assert_eq!(two_128.sqr(), Fe::from_u64(0x1_0000_03d1));
    }

    #[test]
    fn inverse_times_value_is_one() {
        assert!(Fe::ZERO.inverse().is_none());
        for hex in [A_HEX, CV_HEX, P_MINUS_1_HEX] {
            let x = fe(hex);
            let inv = x.inverse().unwrap();
            assert_eq!(x.mul(&inv), Fe::ONE, "{hex}");
        }
        for small in [1u64, 2, 3, 7, 977, u64::MAX] {
            let x = Fe::from_u64(small);
            assert_eq!(x.mul(&x.inverse().unwrap()), Fe::ONE, "{small}");
        }
        // Independent expected values (Python `pow(x, -1, p)`).
        assert_eq!(
            Fe::from_u64(2).inverse().unwrap(),
            fe("7fffffffffffffffffffffffffffffffffffffffffffffffffffffff7ffffe18")
        );
        assert_eq!(
            fe(A_HEX).inverse().unwrap(),
            fe("cbea0c25eda11cb33d402869f994dd841ab14efd09900f0ed730a3b97feb1097")
        );
        assert_eq!(
            fe(CV_HEX).inverse().unwrap(),
            fe("7cb55550415560a93f509b0bb779ee8b9059c5accd05d640cb74f36b20405a6d")
        );
        assert_eq!(Fe::ONE.inverse().unwrap(), Fe::ONE);
    }

    #[test]
    fn sqrt_recovers_a_root_or_rejects_a_non_residue() {
        assert_eq!(Fe::ZERO.sqrt(), Some(Fe::ZERO));
        assert_eq!(Fe::ONE.sqrt().unwrap().sqr(), Fe::ONE);
        for hex in [A_HEX, CV_HEX, P_MINUS_1_HEX] {
            let x = fe(hex);
            let root = x.sqr().sqrt().unwrap();
            assert!(
                root == x || root == x.negate(),
                "sqrt(x^2) must be ±x: {hex}"
            );
            assert_eq!(root.sqr(), x.sqr(), "{hex}");
        }
        // 3, 5, 6, 7 and 10 are quadratic non-residues mod p; 2, 8 and 11 are
        // residues (Legendre symbol computed independently).
        for nqr in [3u64, 5, 6, 7, 10] {
            assert!(
                Fe::from_u64(nqr).sqrt().is_none(),
                "{nqr} must be a non-residue"
            );
        }
        for qr in [2u64, 8, 11] {
            let r = Fe::from_u64(qr).sqrt().expect("residue");
            assert_eq!(r.sqr(), Fe::from_u64(qr), "{qr}");
        }
        // The chain's root for a^2 is p-a here, which pins the concrete value.
        assert_eq!(
            fe(A_HEX).sqr().sqrt().unwrap(),
            fe("d5e1b380ff00c462aa99887766554433221100ffeeddccbbaa99887666554063")
        );
    }

    #[test]
    fn parity_and_zero_predicates() {
        assert!(Fe::ZERO.is_zero());
        assert!(!Fe::ONE.is_zero());
        assert!(!fe(A_HEX).is_zero());
        assert!(Fe::ONE.is_odd());
        assert!(!Fe::ZERO.is_odd());
        assert!(!Fe::from_u64(2).is_odd());
        // p-1 is even (p is odd), p-2 is odd.
        assert!(!fe(P_MINUS_1_HEX).is_odd());
        assert!(fe(P_MINUS_1_HEX).sub(&Fe::ONE).is_odd());
        // A high limb alone must not confuse parity.
        assert!(!fe("8000000000000000000000000000000000000000000000000000000000000000").is_odd());
    }

    #[test]
    fn select_is_a_mask_not_a_branch() {
        let a = fe(A_HEX);
        let b = fe(CV_HEX);
        assert_eq!(Fe::select(&a, &b, true), b);
        assert_eq!(Fe::select(&a, &b, false), a);
    }

    #[test]
    fn field_axioms_hold_on_a_deterministic_walk() {
        // A cheap deterministic sweep: iterate x -> x^2 + 7 and check the ring
        // identities that a carry bug breaks. No RNG exists in this crate, so
        // the walk is the entropy.
        let mut x = fe(A_HEX);
        let mut y = fe(CV_HEX);
        for _ in 0..64 {
            x = x.sqr().add(&Fe::from_u64(7));
            y = y.mul(&x).add(&Fe::ONE);
            // distributivity
            assert_eq!(x.mul(&y.add(&Fe::ONE)), x.mul(&y).add(&x));
            // (x+y)(x-y) = x^2 - y^2
            assert_eq!(x.add(&y).mul(&x.sub(&y)), x.sqr().sub(&y.sqr()));
            // inverse round trip
            if !x.is_zero() {
                assert_eq!(x.mul(&x.inverse().unwrap()), Fe::ONE);
            }
        }
    }
}
