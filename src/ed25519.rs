//! Ed25519 signature verification (RFC 8032, section 5.1.7), implemented in-house.
//!
//! The field is GF(2^255 - 19) held as four little-endian 64-bit limbs that are only reduced
//! on demand; points use extended twisted-Edwards coordinates with the complete addition law,
//! so every point (including the identity and small-order points) takes the same code path.
//! Scalar multiplication is plain double-and-add: verification handles only public data, so
//! constant-time execution is not needed. Signing exists in tests only.

use crate::hash::sha512;

type Fe = [u64; 4];

const P: Fe = [
    0xffff_ffff_ffff_ffed,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
    0x7fff_ffff_ffff_ffff,
];
/// The group order L = 2^252 + 27742317777372353535851937790883648493.
const L: Fe = [
    0x5812_631a_5cf5_d3ed,
    0x14de_f9de_a2f7_9cd6,
    0,
    0x1000_0000_0000_0000,
];
const ZERO: Fe = [0, 0, 0, 0];
const ONE: Fe = [1, 0, 0, 0];

fn fold(mut r: Fe, mut c: u64) -> Fe {
    // 2^256 = 38 (mod p)
    while c != 0 {
        let cur = r[0] as u128 + (c as u128) * 38;
        r[0] = cur as u64;
        let mut cr = (cur >> 64) as u64;
        for limb in r.iter_mut().skip(1) {
            let s = *limb as u128 + cr as u128;
            *limb = s as u64;
            cr = (s >> 64) as u64;
        }
        c = cr;
    }
    r
}

fn add(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut c = 0u64;
    for i in 0..4 {
        let s = a[i] as u128 + b[i] as u128 + c as u128;
        r[i] = s as u64;
        c = (s >> 64) as u64;
    }
    fold(r, c)
}

fn sub(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut br = false;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(br as u64);
        r[i] = d2;
        br = b1 | b2;
    }
    while br {
        // The value wrapped by 2^256 = 38 (mod p): take 38 back off.
        let (d, mut b) = r[0].overflowing_sub(38);
        r[0] = d;
        for limb in r.iter_mut().skip(1) {
            if !b {
                break;
            }
            let (d, bb) = limb.overflowing_sub(1);
            *limb = d;
            b = bb;
        }
        br = b;
    }
    r
}

fn mul(a: &Fe, b: &Fe) -> Fe {
    let mut t = [0u64; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let cur = t[i + j] as u128 + (a[i] as u128) * (b[j] as u128) + carry;
            t[i + j] = cur as u64;
            carry = cur >> 64;
        }
        t[i + 4] = carry as u64;
    }
    let mut r = [0u64; 4];
    let mut carry = 0u128;
    for i in 0..4 {
        let cur = t[i] as u128 + (t[i + 4] as u128) * 38 + carry;
        r[i] = cur as u64;
        carry = cur >> 64;
    }
    fold(r, carry as u64)
}

fn ge(a: &Fe, b: &Fe) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn sub_raw(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut br = false;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(br as u64);
        r[i] = d2;
        br = b1 | b2;
    }
    r
}

/// Fully reduced representative in [0, p).
fn canon(a: &Fe) -> Fe {
    let mut r = *a;
    while ge(&r, &P) {
        r = sub_raw(&r, &P);
    }
    r
}

fn feq(a: &Fe, b: &Fe) -> bool {
    canon(a) == canon(b)
}

fn neg(a: &Fe) -> Fe {
    sub(&ZERO, a)
}

fn pow(base: &Fe, exp: &Fe) -> Fe {
    let mut acc = ONE;
    for i in (0..256).rev() {
        acc = mul(&acc, &acc);
        if (exp[i / 64] >> (i % 64)) & 1 == 1 {
            acc = mul(&acc, base);
        }
    }
    acc
}

fn inv(a: &Fe) -> Fe {
    // a^(p-2)
    pow(
        a,
        &[
            0xffff_ffff_ffff_ffeb,
            0xffff_ffff_ffff_ffff,
            0xffff_ffff_ffff_ffff,
            0x7fff_ffff_ffff_ffff,
        ],
    )
}

fn from_le(bytes: &[u8; 32]) -> Fe {
    let mut r = [0u64; 4];
    for (i, limb) in r.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[8 * i..8 * i + 8]);
        *limb = u64::from_le_bytes(b);
    }
    r
}

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

struct Curve {
    d: Fe,
    d2: Fe,
    sqrt_m1: Fe,
}

impl Curve {
    fn new() -> Self {
        // d = -121665 / 121666
        let d = mul(&neg(&[121665, 0, 0, 0]), &inv(&[121666, 0, 0, 0]));
        // sqrt(-1) = 2^((p-1)/4)
        let sqrt_m1 = pow(
            &[2, 0, 0, 0],
            &[
                0xffff_ffff_ffff_fffb,
                0xffff_ffff_ffff_ffff,
                0xffff_ffff_ffff_ffff,
                0x1fff_ffff_ffff_ffff,
            ],
        );
        Curve {
            d,
            d2: add(&d, &d),
            sqrt_m1,
        }
    }

    fn identity() -> Point {
        Point {
            x: ZERO,
            y: ONE,
            z: ONE,
            t: ZERO,
        }
    }

    fn add(&self, p: &Point, q: &Point) -> Point {
        let a = mul(&sub(&p.y, &p.x), &sub(&q.y, &q.x));
        let b = mul(&add(&p.y, &p.x), &add(&q.y, &q.x));
        let c = mul(&mul(&p.t, &self.d2), &q.t);
        let zz = mul(&p.z, &q.z);
        let d = add(&zz, &zz);
        let e = sub(&b, &a);
        let f = sub(&d, &c);
        let g = add(&d, &c);
        let h = add(&b, &a);
        Point {
            x: mul(&e, &f),
            y: mul(&g, &h),
            z: mul(&f, &g),
            t: mul(&e, &h),
        }
    }

    /// `scalar` is little-endian bytes.
    fn scalar_mul(&self, p: &Point, scalar: &[u8]) -> Point {
        let mut acc = Self::identity();
        for i in (0..scalar.len() * 8).rev() {
            acc = self.add(&acc, &acc);
            if (scalar[i / 8] >> (i % 8)) & 1 == 1 {
                acc = self.add(&acc, p);
            }
        }
        acc
    }

    /// RFC 8032 section 5.1.3 point decoding; `None` for anything that is not a canonical point encoding.
    fn decompress(&self, enc: &[u8; 32]) -> Option<Point> {
        let sign = enc[31] >> 7;
        let mut yb = *enc;
        yb[31] &= 0x7f;
        let y = from_le(&yb);
        if ge(&y, &P) {
            return None;
        }
        let y2 = mul(&y, &y);
        let u = sub(&y2, &ONE);
        let v = add(&mul(&self.d, &y2), &ONE);
        let v3 = mul(&mul(&v, &v), &v);
        let v7 = mul(&mul(&v3, &v3), &v);
        // (p-5)/8
        let e = pow(
            &mul(&u, &v7),
            &[
                0xffff_ffff_ffff_fffd,
                0xffff_ffff_ffff_ffff,
                0xffff_ffff_ffff_ffff,
                0x0fff_ffff_ffff_ffff,
            ],
        );
        let mut x = mul(&mul(&u, &v3), &e);
        let vx2 = mul(&v, &mul(&x, &x));
        if feq(&vx2, &u) {
            // x is a root
        } else if feq(&vx2, &neg(&u)) {
            x = mul(&x, &self.sqrt_m1);
        } else {
            return None;
        }
        let xc = canon(&x);
        if xc == ZERO && sign == 1 {
            return None;
        }
        if (xc[0] & 1) as u8 != sign {
            x = neg(&x);
        }
        Some(Point {
            x,
            y,
            z: ONE,
            t: mul(&x, &y),
        })
    }

    fn base(&self) -> Point {
        // y = 4/5, positive x
        let mut enc = [0x66u8; 32];
        enc[0] = 0x58;
        self.decompress(&enc).expect("base point encoding is valid")
    }
}

fn points_equal(p: &Point, q: &Point) -> bool {
    feq(&mul(&p.x, &q.z), &mul(&q.x, &p.z)) && feq(&mul(&p.y, &q.z), &mul(&q.y, &p.z))
}

fn add_mod_l(a: &Fe, b: &Fe) -> Fe {
    // Both operands are < L < 2^253, so the sum never overflows 256 bits.
    let mut r = [0u64; 4];
    let mut c = 0u64;
    for i in 0..4 {
        let s = a[i] as u128 + b[i] as u128 + c as u128;
        r[i] = s as u64;
        c = (s >> 64) as u64;
    }
    if ge(&r, &L) {
        r = sub_raw(&r, &L);
    }
    r
}

/// Little-endian bytes reduced modulo L.
fn reduce_mod_l(bytes: &[u8]) -> Fe {
    let mut r = ZERO;
    for i in (0..bytes.len() * 8).rev() {
        r = add_mod_l(&r, &r);
        if (bytes[i / 8] >> (i % 8)) & 1 == 1 {
            r = add_mod_l(&r, &ONE);
        }
    }
    r
}

fn fe_to_le(a: &Fe) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[8 * i..8 * i + 8].copy_from_slice(&a[i].to_le_bytes());
    }
    out
}

/// Verify `sig` over `msg` under the 32-byte public key `pk` (RFC 8032, pure Ed25519).
pub(crate) fn verify(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let curve = Curve::new();
    let mut r_enc = [0u8; 32];
    r_enc.copy_from_slice(&sig[..32]);
    let mut s_enc = [0u8; 32];
    s_enc.copy_from_slice(&sig[32..]);
    let s = from_le(&s_enc);
    if ge(&s, &L) {
        return false;
    }
    let a_pt = match curve.decompress(pk) {
        Some(p) => p,
        None => return false,
    };
    let r_pt = match curve.decompress(&r_enc) {
        Some(p) => p,
        None => return false,
    };
    let mut input = Vec::with_capacity(64 + msg.len());
    input.extend_from_slice(&r_enc);
    input.extend_from_slice(pk);
    input.extend_from_slice(msg);
    let h = reduce_mod_l(&sha512(&input));
    let lhs = curve.scalar_mul(&curve.base(), &s_enc);
    let rhs = curve.add(&r_pt, &curve.scalar_mul(&a_pt, &fe_to_le(&h)));
    points_equal(&lhs, &rhs)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    fn compress(curve: &Curve, p: &Point) -> [u8; 32] {
        let _ = curve;
        let zi = inv(&p.z);
        let x = canon(&mul(&p.x, &zi));
        let y = canon(&mul(&p.y, &zi));
        let mut out = fe_to_le(&y);
        out[31] |= ((x[0] & 1) as u8) << 7;
        out
    }

    fn mul_mod_l(a: &Fe, b: &Fe) -> Fe {
        let mut r = ZERO;
        for i in (0..256).rev() {
            r = add_mod_l(&r, &r);
            if (b[i / 64] >> (i % 64)) & 1 == 1 {
                r = add_mod_l(&r, a);
            }
        }
        r
    }

    /// The public key for a 32-byte seed.
    pub(crate) fn public_key(seed: &[u8; 32]) -> [u8; 32] {
        let curve = Curve::new();
        let h = sha512(seed);
        let mut a = [0u8; 32];
        a.copy_from_slice(&h[..32]);
        a[0] &= 248;
        a[31] &= 127;
        a[31] |= 64;
        compress(&curve, &curve.scalar_mul(&curve.base(), &a))
    }

    /// RFC 8032 section 5.1.6 signing, for tests.
    pub(crate) fn sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
        let curve = Curve::new();
        let h = sha512(seed);
        let mut a = [0u8; 32];
        a.copy_from_slice(&h[..32]);
        a[0] &= 248;
        a[31] &= 127;
        a[31] |= 64;
        let pk = compress(&curve, &curve.scalar_mul(&curve.base(), &a));
        let mut ri = h[32..].to_vec();
        ri.extend_from_slice(msg);
        let r = reduce_mod_l(&sha512(&ri));
        let r_enc = compress(&curve, &curve.scalar_mul(&curve.base(), &fe_to_le(&r)));
        let mut ki = r_enc.to_vec();
        ki.extend_from_slice(&pk);
        ki.extend_from_slice(msg);
        let k = reduce_mod_l(&sha512(&ki));
        let a_mod = reduce_mod_l(&a);
        let s = add_mod_l(&r, &mul_mod_l(&k, &a_mod));
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&r_enc);
        sig[32..].copy_from_slice(&fe_to_le(&s));
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{public_key, sign};
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }
    fn arr<const N: usize>(s: &str) -> [u8; N] {
        let v = unhex(s);
        let mut a = [0u8; N];
        a.copy_from_slice(&v);
        a
    }

    // RFC 8032 section 7.1: (secret key, public key, message, signature).
    const VECTORS: [(&str, &str, &str, &str); 3] = [
        (
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            "72",
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        ),
        (
            "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
            "fc51cd8e6218a0a38da47978f6ed8f2b3c2d1c2d5d3b4e5e3f8c1a6a9a4b2a0b",
            "af82",
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        ),
    ];

    #[test]
    fn rfc8032_vectors_verify_and_sign_matches() {
        for (i, (sk, pk, msg, sig)) in VECTORS.iter().enumerate() {
            let sk: [u8; 32] = arr(sk);
            let msg = unhex(msg);
            let sig: [u8; 64] = arr(sig);
            let derived = public_key(&sk);
            if i < 2 {
                assert_eq!(derived, arr::<32>(pk), "vector {i} public key");
            }
            assert_eq!(sign(&sk, &msg), sig, "vector {i} signature");
            assert!(verify(&derived, &msg, &sig), "vector {i} verifies");
        }
    }

    #[test]
    fn rejects_tampering() {
        let (sk, _, msg, sig) = VECTORS[1];
        let sk: [u8; 32] = arr(sk);
        let pk = public_key(&sk);
        let msg = unhex(msg);
        let sig: [u8; 64] = arr(sig);
        assert!(verify(&pk, &msg, &sig));
        assert!(!verify(&pk, b"73", &sig));
        let mut bad = sig;
        bad[0] ^= 1;
        assert!(!verify(&pk, &msg, &bad));
        let mut bad = sig;
        bad[40] ^= 1;
        assert!(!verify(&pk, &msg, &bad));
        // S not below the group order
        let mut bad = sig;
        bad[63] = 0xff;
        assert!(!verify(&pk, &msg, &bad));
        // a different key
        assert!(!verify(&public_key(&[7u8; 32]), &msg, &sig));
        // an undecodable key (y = p + 1 is not canonical)
        let mut weird = [0xffu8; 32];
        weird[31] = 0x7f;
        assert!(!verify(&weird, &msg, &sig));
    }

    #[test]
    fn sign_verify_roundtrip_fresh_keys() {
        for n in 0..4u8 {
            let seed = [n.wrapping_mul(37).wrapping_add(11); 32];
            let pk = public_key(&seed);
            let msg = format!("message {n}");
            let sig = sign(&seed, msg.as_bytes());
            assert!(verify(&pk, msg.as_bytes(), &sig));
        }
    }
}
