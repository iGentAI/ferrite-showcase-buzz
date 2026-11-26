//! The secp256k1 group: the points of `y² = x³ + 7` over GF(p), plus the scalar
//! multiplication BIP-340 is built on.
//!
//! # Provenance
//!
//! Curve parameters (`a = 0`, `b = 7`, the generator `G`) are the ones fixed by
//! SEC 2 v2 §2.4.1. The group law is the standard Jacobian arithmetic catalogued
//! in the Explicit-Formulas Database: `dbl-2009-l` for doubling — which is the
//! `a = 0` specialisation, so two multiplications by `a` vanish — and
//! `add-1998-cmo-2` (Cohen–Miyaji–Ono) for addition. Both are restated in the
//! doc comments of [`Point::double`] and [`Point::add`] in the exact order this
//! code evaluates them. [`Point::lift_x`] is BIP-340's `lift_x`, unmodified.
//!
//! # Representation
//!
//! Jacobian projective coordinates: `(X, Y, Z)` denotes the affine point
//! `(X/Z², Y/Z³)`, and `Z = 0` is the point at infinity. Projective coordinates
//! exist to keep the group law inversion-free — one field inversion at the end
//! of a scalar multiplication instead of one per addition, and a field inversion
//! is 270 multiplications here (255 squarings and 15 products, the addition
//! chain in [`Fe::inverse`]). The price is that the representation is
//! not unique: `(X, Y, Z)` and `(λ²X, λ³Y, λZ)` are the same point. Two
//! consequences run through this module:
//!
//! * [`PartialEq`] is a cross multiplication, not a field-by-field comparison.
//! * The identity is *any* triple with `Z = 0`; `X` and `Y` may be arbitrary
//!   leftovers, so nothing may read them without first checking
//!   [`Point::is_identity`]. Every operation here is closed over that: doubling
//!   a `Z = 0` point keeps `Z = 0`, addition selects the other operand, and
//!   [`Point::to_affine`] refuses.
//!
//! # Side-channel posture
//!
//! [`Point::mul_scalar`] is a fixed 256-iteration double-and-add-always ladder:
//! every iteration performs one doubling and one addition regardless of the bit,
//! and the bit only drives a masked field-limb selection ([`Fe::select`]). There
//! is no early exit, so the loop's shape does not reveal the scalar's bit length,
//! and no allocation, so no allocator behaviour depends on it either. The
//! degenerate cases of the addition law (an identity operand, or equal points)
//! are also resolved by masked selection rather than by branching, which costs
//! one extra doubling per addition and is why the ladder can be trusted with a
//! secret scalar.
//!
//! What is *not* claimed: nothing here is hardened against a hardware attacker
//! who can observe individual multiplication operands (no blinding, no
//! randomised projective coordinates — the crate has no entropy source), and
//! Rust/LLVM make no formal constant-time guarantee. The threat model is remote
//! timing of a server that signs with a long-lived key; see
//! [`crate::crypto::schnorr`].

use crate::crypto::field::Fe;
use crate::crypto::scalar::Scalar;

/// The curve constant `b = 7`. secp256k1 has `a = 0`, which is what licenses the
/// doubling formula used below.
const B: Fe = Fe::from_u64(7);

/// Generator x-coordinate,
/// `79be667e f9dcbbac 55a06295 ce870b07 029bfcdb 2dce28d9 59f2815b 16f81798`
/// (SEC 2 v2 §2.4.1), as little-endian limbs. Pinned against that big-endian
/// encoding in the tests.
const GX: Fe = Fe::from_limbs([
    0x59f2_815b_16f8_1798,
    0x029b_fcdb_2dce_28d9,
    0x55a0_6295_ce87_0b07,
    0x79be_667e_f9dc_bbac,
]);

/// Generator y-coordinate,
/// `483ada77 26a3c465 5da4fbfc 0e1108a8 fd17b448 a6855419 9c47d08f fb10d4b8`.
const GY: Fe = Fe::from_limbs([
    0x9c47_d08f_fb10_d4b8,
    0xfd17_b448_a685_5419,
    0x5da4_fbfc_0e11_08a8,
    0x483a_da77_26a3_c465,
]);

/// A point of the secp256k1 group in Jacobian coordinates.
///
/// Copy semantics: a point is 96 bytes of public-or-secret field data with no
/// invariant beyond "on the curve", which callers get by construction from
/// [`Point::generator`], [`Point::lift_x`] and the group operations.
#[derive(Clone, Copy, Debug)]
pub struct Point {
    x: Fe,
    y: Fe,
    /// `Z = 0` marks the point at infinity; then `x` and `y` are meaningless.
    z: Fe,
}

impl Point {
    /// The standard generator `G` of the group, of prime order
    /// `n` (see [`crate::crypto::scalar`]).
    #[must_use]
    pub const fn generator() -> Self {
        Self {
            x: GX,
            y: GY,
            z: Fe::ONE,
        }
    }

    /// The point at infinity, the group identity.
    ///
    /// Returned in the conventional `(1, 1, 0)` form, but no caller may rely on
    /// that: addition produces identities with arbitrary `X`/`Y`.
    #[must_use]
    pub const fn identity() -> Self {
        Self {
            x: Fe::ONE,
            y: Fe::ONE,
            z: Fe::ZERO,
        }
    }

    /// Whether this is the point at infinity.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.z.is_zero()
    }

    /// Whether the point satisfies the curve equation — `Y² = X³ + 7·Z⁶` in
    /// Jacobian form, which is the affine equation multiplied through by `Z⁶`.
    ///
    /// The identity is reported as on-curve: it is a group element, just not one
    /// the affine equation describes. Nothing in signature verification needs
    /// this (a point reaches the group law only via [`Point::lift_x`], which
    /// solves the curve equation, or via the group law itself), so it exists for
    /// tests and for callers validating externally supplied coordinates.
    #[must_use]
    pub fn is_on_curve(&self) -> bool {
        if self.is_identity() {
            return true;
        }
        let z6 = self.z.sqr().cube();
        self.y.sqr() == self.x.cube().add(&B.mul(&z6))
    }

    /// The inverse of this point, `(X, -Y, Z)`: negating `y` is exactly negating
    /// the group element, since the curve is symmetric about the x-axis.
    #[must_use]
    pub fn negate(&self) -> Self {
        Self {
            x: self.x,
            y: self.y.negate(),
            z: self.z,
        }
    }

    /// `2·self`.
    ///
    /// EFD `dbl-2009-l` (`a = 0`), evaluated in this order:
    ///
    /// ```text
    /// A = X²        B = Y²        C = B²
    /// D = 2·((X+B)² - A - C)      // = 4·X·Y², one squaring instead of a product
    /// E = 3·A       F = E²
    /// X' = F - 2·D
    /// Y' = E·(D - X') - 8·C
    /// Z' = 2·Y·Z
    /// ```
    ///
    /// Total: 7 field multiplications (5 of them squarings), no inversion, no
    /// branch. Degenerate inputs need no special case: with `Z = 0` the formula
    /// returns `Z' = 0`, and a point with `Y = 0` would too — correctly, since
    /// such a point would have order 2, which this group (of odd prime order)
    /// does not contain.
    #[must_use]
    pub fn double(&self) -> Self {
        let a = self.x.sqr();
        let b = self.y.sqr();
        let c = b.sqr();
        let d = self.x.add(&b).sqr().sub(&a).sub(&c).double();
        let e = a.double().add(&a);
        let f = e.sqr();
        let x3 = f.sub(&d.double());
        let y3 = e.mul(&d.sub(&x3)).sub(&c.double().double().double());
        let z3 = self.y.mul(&self.z).double();
        Self {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    /// `self + other`.
    ///
    /// EFD `add-1998-cmo-2`, evaluated in this order:
    ///
    /// ```text
    /// Z1Z1 = Z1²          Z2Z2 = Z2²
    /// U1 = X1·Z2Z2        U2 = X2·Z1Z1
    /// S1 = Y1·Z2Z2·Z2     S2 = Y2·Z1Z1·Z1
    /// H = U2 - U1         R = S2 - S1
    /// HH = H²             HHH = H·HH        V = U1·HH
    /// X3 = R² - HHH - 2·V
    /// Y3 = R·(V - X3) - S1·HHH
    /// Z3 = Z1·Z2·H
    /// ```
    ///
    /// The formula is a chord construction, so it is wrong exactly where there is
    /// no chord. All three such cases are repaired by masked selection, never by
    /// a branch, so that [`Point::mul_scalar`] can call this with a secret
    /// scalar:
    ///
    /// * `H = 0` and `R = 0` — the same affine point, whose chord is the tangent:
    ///   take [`Point::double`] instead. (Computing that doubling
    ///   unconditionally is the ladder's constant-time tax: 7 multiplications on
    ///   top of the addition's 16.)
    /// * `H = 0` and `R ≠ 0` — opposite points, sum is the identity. The formula
    ///   already yields `Z3 = 0`, so no repair is needed; `X3`/`Y3` are then
    ///   leftovers that [`Point::is_identity`] makes unreadable.
    /// * either operand is the identity — the formula's `Z3 = 0` would be wrong,
    ///   so the other operand is selected.
    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        let z1z1 = self.z.sqr();
        let z2z2 = other.z.sqr();
        let u1 = self.x.mul(&z2z2);
        let u2 = other.x.mul(&z1z1);
        let s1 = self.y.mul(&z2z2).mul(&other.z);
        let s2 = other.y.mul(&z1z1).mul(&self.z);
        let h = u2.sub(&u1);
        let r = s2.sub(&s1);
        let hh = h.sqr();
        let hhh = h.mul(&hh);
        let v = u1.mul(&hh);
        let x3 = r.sqr().sub(&hhh).sub(&v).sub(&v);
        let y3 = r.mul(&v.sub(&x3)).sub(&s1.mul(&hhh));
        let z3 = self.z.mul(&other.z).mul(&h);
        let chord = Self {
            x: x3,
            y: y3,
            z: z3,
        };
        // `&` not `&&`: both predicates are evaluated so the choice is a mask,
        // not a short-circuit branch on a secret intermediate.
        let coincident = h.is_zero() & r.is_zero();
        let out = Self::select(&chord, &self.double(), coincident);
        // Identity operands last, and in this order, so that identity + identity
        // resolves to an identity whatever the chord produced.
        let out = Self::select(&out, other, self.is_identity());
        Self::select(&out, self, other.is_identity())
    }

    /// `k · self`.
    ///
    /// A fixed 256-iteration double-and-add-always ladder, most-significant bit
    /// first: each iteration doubles the accumulator, computes the sum with the
    /// base point, and keeps the sum only if the scalar bit is set — as a masked
    /// selection, not a branch. There is no early exit for `k = 0` or for
    /// leading zero bits, and nothing is allocated, so the work is identical for
    /// every scalar. See the module header for what that does and does not buy.
    #[must_use]
    pub fn mul_scalar(&self, k: &Scalar) -> Self {
        let mut acc = Self::identity();
        for index in (0..256).rev() {
            acc = acc.double();
            let sum = acc.add(self);
            acc = Self::select(&acc, &sum, k.bit(index));
        }
        acc
    }

    /// `k · G`.
    ///
    /// The same generic ladder as [`Point::mul_scalar`]; there is no precomputed
    /// comb table for `G`. A table would cut the `s·G` half of a verification
    /// several-fold, but it is a second code path to audit and a lookup whose
    /// access pattern would have to be argued about, and the app's throughput
    /// requirement is one verification per inbound event, not a batch. Measured
    /// cost as written, release build on aarch64: ≈0.4 ms per verification
    /// (two ladders) and ≈0.8 ms per signature (four, counting the self-check).
    #[must_use]
    pub fn mul_generator(k: &Scalar) -> Self {
        Self::generator().mul_scalar(k)
    }

    /// The affine coordinates as field elements, or `None` for the identity
    /// (which has no affine form).
    ///
    /// Costs one field inversion: `x = X/Z²`, `y = Y/Z³`.
    #[must_use]
    pub fn to_affine_fe(&self) -> Option<(Fe, Fe)> {
        // `inverse` returns `None` exactly for `Z = 0`, i.e. for the identity.
        let z_inv = self.z.inverse()?;
        let z_inv2 = z_inv.sqr();
        Some((self.x.mul(&z_inv2), self.y.mul(&z_inv2).mul(&z_inv)))
    }

    /// The affine coordinates as 32-byte big-endian integers `(x, y)`, or `None`
    /// for the identity.
    ///
    /// The encoding is canonical (both coordinates are reduced representatives),
    /// which is what makes a byte-for-byte comparison against a signature's `r`
    /// component sound.
    #[must_use]
    pub fn to_affine(&self) -> Option<([u8; 32], [u8; 32])> {
        let (x, y) = self.to_affine_fe()?;
        Some((x.to_bytes_be(), y.to_bytes_be()))
    }

    /// Whether the affine `y` is even — BIP-340's `has_even_y` — or `None` for
    /// the identity, for which the predicate is undefined.
    ///
    /// Returning `None` rather than a default is deliberate: BIP-340 test
    /// vectors 9 and 10 exist precisely to catch verifiers that treat the
    /// identity as having an even `y` (and `x = 0`), which would accept a forged
    /// signature.
    #[must_use]
    pub fn has_even_y(&self) -> Option<bool> {
        let (_, y) = self.to_affine_fe()?;
        Some(!y.is_odd())
    }

    /// BIP-340 `lift_x`: the point with x-coordinate `x` and **even** `y`, or
    /// `None` if there is none.
    ///
    /// Fails when `x >= p` (a non-canonical encoding is not a different point)
    /// and when `x³ + 7` is not a quadratic residue (`x` is not on the curve).
    /// `x = 0` fails for the second reason, since 7 is a non-residue mod p, so
    /// the all-zero x-only public key is rejected without a special case.
    #[must_use]
    pub fn lift_x(x: &[u8; 32]) -> Option<Self> {
        let x = Fe::from_bytes_be(x)?;
        let y = x.cube().add(&B).sqrt()?;
        // Of the two roots ±y, take the even one. `y = 0` cannot occur: it would
        // be a point of order 2, and the group order is odd.
        let y = Fe::select(&y, &y.negate(), y.is_odd());
        Some(Self { x, y, z: Fe::ONE })
    }

    /// Branch-free selection: `b` when `pick_b`, else `a`.
    fn select(a: &Self, b: &Self, pick_b: bool) -> Self {
        Self {
            x: Fe::select(&a.x, &b.x, pick_b),
            y: Fe::select(&a.y, &b.y, pick_b),
            z: Fe::select(&a.z, &b.z, pick_b),
        }
    }
}

/// Projective equality: `(X₁, Y₁, Z₁) == (X₂, Y₂, Z₂)` iff they denote the same
/// affine point, tested as `X₁Z₂² = X₂Z₁²` and `Y₁Z₂³ = Y₂Z₁³` so that no
/// inversion is needed. All identities are equal to each other and to nothing
/// else.
///
/// This is a *representation-independent* comparison, which is the only correct
/// one for Jacobian coordinates; a derived `PartialEq` would report two encodings
/// of one point as different.
///
/// Unlike the group law, this short-circuits, so it is not constant-time. That is
/// deliberate and safe here: nothing on the signing or verification path compares
/// points (verification compares a field element against the signature's `r`),
/// and the comparison exists for tests and for callers checking public values.
impl PartialEq for Point {
    fn eq(&self, other: &Self) -> bool {
        if self.is_identity() || other.is_identity() {
            return self.is_identity() && other.is_identity();
        }
        let z1z1 = self.z.sqr();
        let z2z2 = other.z.sqr();
        self.x.mul(&z2z2) == other.x.mul(&z1z1)
            && self.y.mul(&z2z2).mul(&other.z) == other.y.mul(&z1z1).mul(&self.z)
    }
}

impl Eq for Point {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::crypto::{from_hex32, to_hex};

    /// Affine coordinates of `k·G` for small `k`, and of `(n-1)·G`, computed
    /// independently in Python's arbitrary-precision integers over the SEC 2
    /// parameters (`/tmp` scratch reference, not this code).
    const G_HEX: (&str, &str) = (
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8",
    );
    const G2_HEX: (&str, &str) = (
        "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
        "1ae168fea63dc339a3c58419466ceaeef7f632653266d0e1236431a950cfe52a",
    );
    const G3_HEX: (&str, &str) = (
        "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
        "388f7b0f632de8140fe337e62a37f3566500a99934c2231b6cb9fd7584b8e672",
    );
    const G4_HEX: (&str, &str) = (
        "e493dbf1c10d80f3581e4904930b1404cc6c13900ee0758474fa94abe8c4cd13",
        "51ed993ea0d455b75642e2098ea51448d967ae33bfbdfe40cfe97bdc47739922",
    );
    const G5_HEX: (&str, &str) = (
        "2f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4",
        "d8ac222636e5e3d6d4dba9dda6c9c426f788271bab0d6840dca87d3aa6ac62d6",
    );
    /// `(n-1)·G = -G`: same x, and the odd one of the two y values.
    const G_NEG_HEX: (&str, &str) = (
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        "b7c52588d95c3b9aa25b0403f1eef75702e84bb7597aabe663b82f6f04ef2777",
    );
    /// `k·G` for `k = 1e2b3c4d…9e0f1a2b`, a full-width scalar that exercises the
    /// whole ladder.
    const K_HEX: &str = "1e2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
    const KG_HEX: (&str, &str) = (
        "8d85953c8305d29961f4e8886f543e329ed5ca1df0cac8283ccbdf18ce5e24b8",
        "b38c341fdfbc17cfb3518646140a5b2b2e6dc1fe58e6a8c0fb80d1094afe615c",
    );

    const P_HEX: &str = "fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f";
    const N_MINUS_1_HEX: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140";

    fn fe(hex: &str) -> Fe {
        Fe::from_bytes_be(&from_hex32(hex).unwrap()).unwrap()
    }

    fn scalar(hex: &str) -> Scalar {
        Scalar::from_bytes_be_canonical(&from_hex32(hex).unwrap()).unwrap()
    }

    /// A point from affine coordinates. Test-only: the library builds points
    /// through `lift_x` and the group law, which cannot produce an off-curve
    /// point, so there is no public affine constructor to validate.
    fn affine(hex: (&str, &str)) -> Point {
        Point {
            x: fe(hex.0),
            y: fe(hex.1),
            z: Fe::ONE,
        }
    }

    fn assert_affine_eq(point: &Point, want: (&str, &str), label: &str) {
        let (x, y) = point.to_affine().expect("not the identity");
        assert_eq!(to_hex(&x), want.0, "{label}: x");
        assert_eq!(to_hex(&y), want.1, "{label}: y");
    }

    #[test]
    fn generator_limbs_match_the_standard_big_endian_encoding() {
        assert_eq!(GX, fe(G_HEX.0));
        assert_eq!(GY, fe(G_HEX.1));
        assert_eq!(
            B,
            fe("0000000000000000000000000000000000000000000000000000000000000007")
        );
        assert_affine_eq(&Point::generator(), G_HEX, "G");
    }

    #[test]
    fn small_multiples_are_on_the_curve() {
        assert!(
            Point::identity().is_on_curve(),
            "identity counts as on-curve"
        );
        let mut acc = Point::identity();
        for step in 1..=8u64 {
            acc = acc.add(&Point::generator());
            assert!(acc.is_on_curve(), "{step}G must satisfy the curve equation");
        }
        // An off-curve triple must be rejected, or the predicate proves nothing.
        let bogus = affine((G_HEX.0, G2_HEX.1));
        assert!(!bogus.is_on_curve());
    }

    #[test]
    fn identity_is_the_group_identity() {
        let id = Point::identity();
        let g = Point::generator();
        assert!(id.is_identity());
        assert!(!g.is_identity());
        assert_eq!(id.to_affine(), None, "infinity has no affine form");
        assert_eq!(id.to_affine_fe(), None);
        assert_eq!(id.has_even_y(), None, "parity is undefined at infinity");
        assert_eq!(id.add(&g), g);
        assert_eq!(g.add(&id), g);
        assert_eq!(id.add(&id), id);
        assert!(id.double().is_identity());
        assert!(id.mul_scalar(&scalar(K_HEX)).is_identity());
        assert!(id.negate().is_identity());
    }

    #[test]
    fn doubling_and_addition_agree() {
        let g = Point::generator();
        let two_g = g.add(&g);
        assert_eq!(two_g, g.double(), "G+G must equal 2G");
        assert_affine_eq(&two_g, G2_HEX, "2G");
        // 3G three ways: 2G+G, G+2G, and the ladder.
        let three_g = two_g.add(&g);
        assert_eq!(three_g, g.add(&two_g));
        assert_eq!(three_g, g.mul_scalar(&Scalar::from_u64(3)));
        assert_affine_eq(&three_g, G3_HEX, "3G");
        // 4G as 2·(2G) and as 3G+G.
        assert_eq!(two_g.double(), three_g.add(&g));
        assert_affine_eq(&two_g.double(), G4_HEX, "4G");
        // Addition is commutative and associative on this sample.
        let five_g = two_g.add(&three_g);
        assert_eq!(five_g, three_g.add(&two_g));
        assert_eq!(five_g, two_g.double().add(&g));
        assert_affine_eq(&five_g, G5_HEX, "5G");
    }

    #[test]
    fn addition_of_opposites_is_the_identity() {
        let g = Point::generator();
        assert!(g.add(&g.negate()).is_identity());
        assert_eq!(g.negate().negate(), g);
        assert_affine_eq(&g.negate(), G_NEG_HEX, "-G");
        let three_g = g.mul_scalar(&Scalar::from_u64(3));
        assert!(three_g.add(&three_g.negate()).is_identity());
        // A doubled identity coming out of `add` stays usable as an operand.
        let zero = g.add(&g.negate());
        assert_eq!(zero.add(&g), g);
        assert!(zero.double().is_identity());
    }

    #[test]
    fn mul_scalar_edges_are_exact() {
        let g = Point::generator();
        assert!(g.mul_scalar(&Scalar::ZERO).is_identity(), "0·G is infinity");
        assert_eq!(g.mul_scalar(&Scalar::ONE), g, "1·G is G");
        assert_eq!(g.mul_scalar(&Scalar::from_u64(2)), g.double());
        assert_affine_eq(&g.mul_scalar(&scalar(K_HEX)), KG_HEX, "kG");
        // (n-1)·G = -G, so n·G = (n-1)·G + G is the identity. `n` itself is not a
        // representable `Scalar` (the type is canonical, `< n`), which is why the
        // group order is exercised in this split form.
        let n_minus_1 = g.mul_scalar(&scalar(N_MINUS_1_HEX));
        assert_eq!(n_minus_1, g.negate());
        assert!(n_minus_1.add(&g).is_identity(), "n·G must be infinity");
        assert_affine_eq(&n_minus_1, G_NEG_HEX, "(n-1)G");
    }

    #[test]
    fn mul_scalar_is_a_group_homomorphism() {
        let g = Point::generator();
        let a = scalar(K_HEX);
        let b = scalar("0123456789abcdeffedcba98765432100f1e2d3c4b5a69788796a5b4c3d2e1f0");
        // (a+b)·G = a·G + b·G
        assert_eq!(
            g.mul_scalar(&a.add(&b)),
            g.mul_scalar(&a).add(&g.mul_scalar(&b))
        );
        // (a·b)·G = a·(b·G) — the ladder on a non-generator base point.
        assert_eq!(g.mul_scalar(&a.mul(&b)), g.mul_scalar(&b).mul_scalar(&a));
        // a·G + (-a)·G = infinity
        assert!(g
            .mul_scalar(&a)
            .add(&g.mul_scalar(&a.negate()))
            .is_identity());
        // mul_generator is the ladder on G.
        assert_eq!(Point::mul_generator(&a), g.mul_scalar(&a));
        // Doubling the base commutes with doubling the scalar.
        assert_eq!(g.double().mul_scalar(&a), g.mul_scalar(&a).double());
    }

    #[test]
    fn the_ladder_reads_scalar_bits_most_significant_first() {
        // 2^255·G computed by 255 doublings must match the ladder's answer, which
        // it only does if bit 255 is consumed on the first iteration.
        let mut doubled = Point::generator();
        for _ in 0..255 {
            doubled = doubled.double();
        }
        let two_255 = scalar("8000000000000000000000000000000000000000000000000000000000000000");
        assert_eq!(Point::mul_generator(&two_255), doubled);
        // 2^255 + 1 must be that point plus G, which pins the low bit too.
        assert_eq!(
            Point::mul_generator(&two_255.add(&Scalar::ONE)),
            doubled.add(&Point::generator())
        );
        // A single low bit: 2^8·G is eight doublings.
        let mut small = Point::generator();
        for _ in 0..8 {
            small = small.double();
        }
        assert_eq!(Point::mul_generator(&Scalar::from_u64(256)), small);
    }

    #[test]
    fn ladder_output_stays_on_the_curve_across_a_deterministic_sweep() {
        // No RNG exists in this crate, so the scalars come from a hash chain.
        // Every product must be a real curve point, must lift back from its own
        // x-coordinate, and must satisfy (k+1)·G = k·G + G.
        use crate::crypto::sha256::sha256;
        let mut state = sha256(b"buzz-relay/point/sweep");
        for step in 0..8u8 {
            state = sha256(&state);
            let k = Scalar::from_bytes_be_reduced(&state);
            let q = Point::mul_generator(&k);
            assert!(q.is_on_curve(), "step {step}: k·G must be on the curve");
            assert!(
                !q.is_identity(),
                "step {step}: k·G is not infinity for k != 0"
            );
            let (x, _) = q.to_affine().expect("not the identity");
            let lifted = Point::lift_x(&x).expect("x of a real point lifts");
            assert!(
                lifted == q || lifted == q.negate(),
                "step {step}: lift_x(x(Q)) = ±Q"
            );
            assert_eq!(
                Point::mul_generator(&k.add(&Scalar::ONE)),
                q.add(&Point::generator()),
                "step {step}: (k+1)·G = k·G + G"
            );
        }
    }

    #[test]
    fn lift_x_rejects_out_of_range_and_off_curve_inputs() {
        // x >= p: p itself, p+1 (BIP-340 vector 14's public key), and all-ones.
        assert!(
            Point::lift_x(&from_hex32(P_HEX).unwrap()).is_none(),
            "x = p"
        );
        assert!(
            Point::lift_x(
                &from_hex32("fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc30")
                    .unwrap()
            )
            .is_none(),
            "x = p+1"
        );
        assert!(Point::lift_x(&[0xff; 32]).is_none(), "x = 2^256-1");
        // On-curve test: x^3+7 must be a quadratic residue. Zero is not (7 is a
        // non-residue mod p), and BIP-340 vector 5's public key is not.
        assert!(
            Point::lift_x(&[0u8; 32]).is_none(),
            "x = 0 is not on the curve"
        );
        assert!(
            Point::lift_x(
                &from_hex32("eefdea4cdb677750a420fee807eacf21eb9898ae79b9768766e4faa04a2d4a34")
                    .unwrap()
            )
            .is_none(),
            "BIP-340 vector 5: public key not on the curve"
        );
        // BIP-340 vector 11's r is likewise not an x-coordinate.
        assert!(
            Point::lift_x(
                &from_hex32("4a298dacae57395a15d0795ddbfd1dcb564da82b0f269bc70a74f8220429ba1d")
                    .unwrap()
            )
            .is_none(),
            "BIP-340 vector 11: sig[0:32] is not an x-coordinate"
        );
    }

    #[test]
    fn lift_x_accepts_about_half_of_all_x_values() {
        // Exactly half the non-zero field elements are quadratic residues, so
        // `x³ + 7` is a residue for about half of all x — which means `lift_x`
        // should accept close to 50% of arbitrary 32-byte inputs. A sqrt that
        // accepted non-residues (or rejected residues) would land far outside
        // this band, and no known-answer vector would necessarily catch it.
        // The inputs come from a hash chain, since the crate has no RNG.
        use crate::crypto::sha256::sha256;
        let mut state = sha256(b"buzz-relay/point/lift-rate");
        let mut lifted = 0;
        for _ in 0..256 {
            state = sha256(&state);
            if let Some(point) = Point::lift_x(&state) {
                lifted += 1;
                assert!(point.is_on_curve(), "a lifted point must be on the curve");
                assert_eq!(point.has_even_y(), Some(true));
                assert_eq!(
                    point.to_affine().expect("affine").0,
                    state,
                    "x is preserved"
                );
            }
        }
        assert!(
            (96..=160).contains(&lifted),
            "lift_x accepted {lifted}/256 x values; expected roughly half"
        );
    }

    #[test]
    fn lift_x_returns_the_even_y_point() {
        // G has even y, so lift_x(x(G)) is G itself.
        let lifted = Point::lift_x(&from_hex32(G_HEX.0).unwrap()).expect("G is on the curve");
        assert_eq!(lifted, Point::generator());
        assert_eq!(lifted.has_even_y(), Some(true));
        assert!(lifted.is_on_curve());
        // -G has odd y, and shares x with G: lifting recovers G, i.e. -(-G).
        let neg_g = Point::generator().negate();
        assert_eq!(neg_g.has_even_y(), Some(false));
        let relifted = Point::lift_x(&neg_g.to_affine().unwrap().0).unwrap();
        assert_eq!(relifted, neg_g.negate());
        assert_eq!(relifted.has_even_y(), Some(true));
        // Round trip over several multiples: lift_x(x(Q)) is Q or -Q, always even.
        let mut q = Point::generator();
        for step in 1..=6u64 {
            let (x, _) = q.to_affine().unwrap();
            let l = Point::lift_x(&x).expect("a real point's x lifts");
            assert_eq!(l.has_even_y(), Some(true), "{step}G");
            assert!(l == q || l == q.negate(), "{step}G lifts to ±Q");
            assert_eq!(l.has_even_y() == q.has_even_y(), l == q, "{step}G parity");
            q = q.add(&Point::generator());
        }
    }

    #[test]
    fn has_even_y_matches_the_affine_parity() {
        let mut q = Point::generator();
        for step in 1..=8u64 {
            let (_, y) = q.to_affine().unwrap();
            let even_last_byte = y[31] % 2 == 0;
            assert_eq!(q.has_even_y(), Some(even_last_byte), "{step}G");
            q = q.add(&Point::generator());
        }
        assert_eq!(Point::generator().has_even_y(), Some(true));
        assert_eq!(Point::generator().negate().has_even_y(), Some(false));
    }

    #[test]
    fn equality_ignores_the_projective_scale_factor() {
        let g = Point::generator();
        // Scale by λ = 3: (λ²X, λ³Y, λZ) is the same point.
        let lambda = Fe::from_u64(3);
        let scaled = Point {
            x: g.x.mul(&lambda.sqr()),
            y: g.y.mul(&lambda.cube()),
            z: g.z.mul(&lambda),
        };
        assert_eq!(scaled, g);
        assert!(scaled.is_on_curve());
        assert_eq!(scaled.to_affine(), g.to_affine());
        // Different points are not equal, and neither is a negation.
        assert_ne!(g, g.double());
        assert_ne!(g, g.negate());
        // Identities compare equal whatever their leftover X/Y.
        let junk_identity = Point {
            x: fe(G2_HEX.0),
            y: fe(G2_HEX.1),
            z: Fe::ZERO,
        };
        assert_eq!(junk_identity, Point::identity());
        assert_ne!(junk_identity, g);
        assert_ne!(g, Point::identity());
    }

    #[test]
    fn coincident_addition_uses_the_tangent_even_across_representations() {
        // The `H == 0 && R == 0` repair has to fire for differently scaled
        // encodings of the same point, not just for a literally identical pair.
        let g = Point::generator();
        let lambda = Fe::from_u64(7);
        let scaled = Point {
            x: g.x.mul(&lambda.sqr()),
            y: g.y.mul(&lambda.cube()),
            z: g.z.mul(&lambda),
        };
        assert_eq!(g.add(&scaled), g.double());
        assert_eq!(scaled.add(&g), g.double());
        // And the opposite-points case across representations.
        assert!(g.add(&scaled.negate()).is_identity());
    }
}
