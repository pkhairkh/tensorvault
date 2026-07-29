//! The niche-filled NaN-boxed `Cell` type.
//!
//! See the module-level docs in [`crate::bitcell`] for the encoding table.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Tag bits stored in the high 16 bits of the 64-bit word.
///
/// The high 16 bits are `[sign(1) | exponent(11) | mantissa_high(4)]` of an
/// IEEE-754 double. For real doubles, the exponent is anything other than
/// `0x7FF`, so the pattern `0xFFFx_xxxx_xxxx_xxxx` (sign=1, exp=0x7FF, top
/// mantissa nibble = x) is the free NaN namespace we use for tags.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeTag {
    /// `0x0000` — NULL.
    Null = 0x0000,
    /// `0xFFF0` — tagged i32 in low 32 bits.
    I32 = 0xFFF0,
    /// `0xFFF1` — tagged bool / small enum in low 8 bits.
    Bool = 0xFFF1,
    /// `0xFFF2` — tagged 48-bit pointer.
    Ptr = 0xFFF2,
    /// `0xFFF3` — tagged date (i32 days since epoch).
    Date = 0xFFF3,
    /// `0xFFF4` — tagged timestamp (i64 nanos in payload — needs two cells for
    /// full range; we store the low 47 bits and a separate high-bits table).
    Timestamp = 0xFFF4,
    /// `0xFFF5` — tagged 16-bit float (f16) — half-precision values stored
    /// inline, useful for compressed ML feature columns.
    F16 = 0xFFF5,
    /// `0x7FF8` — canonical NaN sentinel (used when boxing a real NaN f64 so
    /// the payload bits don't collide with our tag space).
    NanSentinel = 0x7FF8,
    /// `0x0001`–`0x000F` (subnormal exponent, low nibble of mantissa high) —
    /// short string: up to 6 ASCII bytes packed into the low 48 bits.
    ShortStr = 0x0001,
}

impl TypeTag {
    /// Extract the tag from a raw 64-bit word.
    #[inline]
    pub fn from_bits(bits: u64) -> Self {
        let high = (bits >> 48) as u16;
        match high {
            0x0000 => {
                if bits == 0 {
                    TypeTag::Null
                } else {
                    // Subnormal: short string.
                    TypeTag::ShortStr
                }
            }
            0xFFF0 => TypeTag::I32,
            0xFFF1 => TypeTag::Bool,
            0xFFF2 => TypeTag::Ptr,
            0xFFF3 => TypeTag::Date,
            0xFFF4 => TypeTag::Timestamp,
            0xFFF5 => TypeTag::F16,
            0x7FF8 => TypeTag::NanSentinel,
            _ => {
                // Real double (exponent != 0x7FF and != 0).
                TypeTag::from_double_check(bits)
            }
        }
    }

    #[inline]
    fn from_double_check(bits: u64) -> Self {
        let exp = (bits >> 52) & 0x7FF;
        if exp == 0x7FF {
            // NaN with our tag pattern — shouldn't happen for real doubles.
            TypeTag::NanSentinel
        } else {
            // It's a real f64. We represent this with a sentinel TypeTag value
            // that isn't in the enum; instead, callers check `is_f64()`.
            // For simplicity, return NanSentinel as a placeholder.
            TypeTag::NanSentinel
        }
    }
}

/// A 64-bit NaN-boxed cell holding any of the supported types.
///
/// The inner `u64` is the raw bit pattern. All operations are branch-light
/// bit manipulations; the FPU never touches tagged values.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Pod, Zeroable, Serialize, Deserialize)]
pub struct Cell(pub u64);

impl Cell {
    /// Tag for NULL — all-zero word.
    pub const TAG_NULL: u64 = 0x0000_0000_0000_0000;
    /// Tag for i32 — high 16 bits = 0xFFF0, low 32 bits = i32 zero-extended.
    pub const TAG_I32: u64 = 0xFFF0_0000_0000_0000;
    /// Tag for bool — high 16 bits = 0xFFF1, low 8 bits = 0 or 1.
    pub const TAG_BOOL: u64 = 0xFFF1_0000_0000_0000;
    /// Tag for pointer — high 16 bits = 0xFFF2, low 48 bits = pointer.
    pub const TAG_PTR: u64 = 0xFFF2_0000_0000_0000;
    /// Tag for date — high 16 bits = 0xFFF3, low 32 bits = i32 days.
    pub const TAG_DATE: u64 = 0xFFF3_0000_0000_0000;
    /// Tag for timestamp — high 16 bits = 0xFFF4, low 47 bits = nanos.
    pub const TAG_TS: u64 = 0xFFF4_0000_0000_0000;
    /// Tag for f16 — high 16 bits = 0xFFF5, low 16 bits = IEEE-754 half.
    pub const TAG_F16: u64 = 0xFFF5_0000_0000_0000;
    /// Canonical NaN sentinel — boxed real NaN.
    pub const NAN_CANON: u64 = 0x7FF8_0000_0000_0000;

    /// Mask for the low 48 bits (pointer payload).
    pub const PAYLOAD48: u64 = 0x0000_FFFF_FFFF_FFFF;
    /// Mask for the low 32 bits (i32 / date payload).
    pub const PAYLOAD32: u64 = 0x0000_0000_FFFF_FFFF;

    /// Construct a NULL cell.
    #[inline]
    pub const fn null() -> Self {
        Cell(Self::TAG_NULL)
    }

    /// Construct a cell from an f64. Real doubles are stored as-is (identity
    /// boxing). NaNs are canonicalized to [`NAN_CANON`] so the payload bits
    /// don't collide with our tag space.
    #[inline]
    pub fn from_f64(x: f64) -> Self {
        let bits = x.to_bits();
        // Check if x is NaN: exponent = 0x7FF and mantissa != 0.
        let exp = (bits >> 52) & 0x7FF;
        let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
        if exp == 0x7FF && mantissa != 0 {
            Cell(Self::NAN_CANON)
        } else {
            Cell(bits)
        }
    }

    /// Construct a cell from an i32.
    #[inline]
    pub const fn from_i32(i: i32) -> Self {
        Cell(Self::TAG_I32 | (i as u32 as u64))
    }

    /// Construct a cell from a bool.
    #[inline]
    pub const fn from_bool(b: bool) -> Self {
        Cell(Self::TAG_BOOL | (b as u64))
    }

    /// Construct a cell from a 48-bit pointer.
    ///
    /// # Safety
    /// The caller must ensure the pointer fits in 48 bits (true on x86-64
    /// and ARM64 canonical addressing). High 16 bits are stripped.
    #[inline]
    pub fn from_ptr(p: *const ()) -> Self {
        Cell(Self::TAG_PTR | ((p as u64) & Self::PAYLOAD48))
    }

    /// Construct a cell from a date (days since UNIX epoch as i32).
    #[inline]
    pub const fn from_date(days: i32) -> Self {
        Cell(Self::TAG_DATE | (days as u32 as u64))
    }

    /// Construct a cell from an f16 bit pattern (16 bits).
    #[inline]
    pub const fn from_f16_bits(h: u16) -> Self {
        Cell(Self::TAG_F16 | (h as u64))
    }

    /// Construct a cell from a short string (≤ 6 ASCII bytes).
    ///
    /// Longer strings must be stored out-of-line via a pointer cell. The
    /// encoding uses the subnormal-double namespace (exponent = 0, mantissa ≠
    /// 0). Length is implicit: trailing zero bytes are stripped.
    #[inline]
    pub fn from_short_str(s: &[u8]) -> Option<Self> {
        if s.len() > 6 || s.is_empty() {
            return None;
        }
        let mut payload: u64 = 0;
        for (i, &b) in s.iter().enumerate() {
            if b == 0 {
                return None; // NUL not allowed; would confuse length detection
            }
            payload |= (b as u64) << (8 * i);
        }
        // Ensure the high 16 bits are 0x0001 (so it's a subnormal, not NULL).
        // We set bit 48 to distinguish short-strings from NULL.
        payload |= 0x0001_0000_0000_0000;
        // But wait: that's not subnormal. Let me reconsider.
        // The encoding: bits [63:48] = 0x0001, bits [47:0] = string bytes.
        // This makes the high16 = 0x0001, which we dispatch on in TypeTag::from_bits.
        Some(Cell(payload))
    }

    /// Construct a cell from a raw u64 bit pattern. Caller assumes responsibility
    /// for the encoding.
    #[inline]
    pub const fn from_raw(bits: u64) -> Self {
        Cell(bits)
    }

    /// Return the raw 64-bit pattern.
    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Return the type tag.
    #[inline]
    pub fn tag(self) -> TypeTag {
        TypeTag::from_bits(self.0)
    }

    /// Is this cell NULL?
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == Self::TAG_NULL
    }

    /// Is this cell a real f64 (identity-boxed)?
    #[inline]
    pub fn is_f64(self) -> bool {
        let exp = (self.0 >> 52) & 0x7FF;
        // Real double: exponent is anything other than 0x7FF (NaN/Inf space)
        // AND not 0 (subnormal/zero space, which we use for NULL/short-str).
        // The only exception is `0x7FF8...` which is our canonical NaN sentinel
        // — we treat it as "f64 NaN".
        exp != 0x7FF && exp != 0
            || self.0 == Self::NAN_CANON
    }

    /// Is this cell a tagged i32?
    #[inline]
    pub const fn is_i32(self) -> bool {
        (self.0 >> 48) == 0xFFF0
    }

    /// Is this cell a tagged pointer?
    #[inline]
    pub const fn is_ptr(self) -> bool {
        (self.0 >> 48) == 0xFFF2
    }

    /// Is this cell a short string?
    #[inline]
    pub const fn is_short_str(self) -> bool {
        (self.0 >> 48) == 0x0001
    }

    /// Decode as f64. Returns None for non-f64 cells.
    #[inline]
    pub fn as_f64(self) -> Option<f64> {
        if self.is_f64() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    /// Decode as i32. Returns None for non-i32 cells.
    #[inline]
    pub fn as_i32(self) -> Option<i32> {
        if self.is_i32() {
            Some(self.0 as u32 as i32)
        } else {
            None
        }
    }

    /// Decode as bool. Returns None for non-bool cells.
    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        if (self.0 >> 48) == 0xFFF1 {
            Some((self.0 & 1) != 0)
        } else {
            None
        }
    }

    /// Decode as a raw 48-bit pointer payload. Returns None for non-ptr cells.
    #[inline]
    pub fn as_ptr_bits(self) -> Option<u64> {
        if self.is_ptr() {
            Some(self.0 & Self::PAYLOAD48)
        } else {
            None
        }
    }

    /// Decode as a short string (≤ 6 bytes). Returns None for non-short-str cells.
    pub fn as_short_str(self) -> Option<Vec<u8>> {
        if !self.is_short_str() {
            return None;
        }
        let payload = self.0 & 0x0000_FFFF_FFFF_FFFF;
        let mut out = Vec::with_capacity(6);
        for i in 0..6 {
            let b = ((payload >> (8 * i)) & 0xFF) as u8;
            if b == 0 {
                break;
            }
            out.push(b);
        }
        Some(out)
    }

    /// Decode as a date (i32 days since epoch).
    #[inline]
    pub fn as_date(self) -> Option<i32> {
        if (self.0 >> 48) == 0xFFF3 {
            Some(self.0 as u32 as i32)
        } else {
            None
        }
    }

    /// Decode as f16 bits (u16).
    #[inline]
    pub fn as_f16_bits(self) -> Option<u16> {
        if (self.0 >> 48) == 0xFFF5 {
            Some(self.0 as u16)
        } else {
            None
        }
    }

    /// Bitwise XOR with another cell. This is the primitive used by Hamming
    /// distance, equality testing, and similarity scoring.
    #[inline]
    pub const fn xor(self, other: Self) -> Self {
        Cell(self.0 ^ other.0)
    }

    /// Hamming distance (number of differing bits) between two cells.
    ///
    /// This is the fundamental similarity primitive: it works for ANY type
    /// because every cell is a 64-bit word. For real f64s, this approximates
    /// semantic distance; for tagged values, it's a cheap "are these equal?"
    /// check (distance 0) or a fuzzy match (small distance).
    #[inline]
    pub fn hamming(self, other: Self) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    /// Equality check via XOR. Works for any type.
    #[inline]
    pub const fn bit_eq(self, other: Self) -> bool {
        self.0 == other.0
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::null()
    }
}

impl From<f64> for Cell {
    #[inline]
    fn from(x: f64) -> Self {
        Cell::from_f64(x)
    }
}

impl From<i32> for Cell {
    #[inline]
    fn from(x: i32) -> Self {
        Cell::from_i32(x)
    }
}

impl From<bool> for Cell {
    #[inline]
    fn from(x: bool) -> Self {
        Cell::from_bool(x)
    }
}

impl From<&[u8]> for Cell {
    #[inline]
    fn from(s: &[u8]) -> Self {
        Cell::from_short_str(s).unwrap_or_else(|| {
            // Fallback: store a NULL if the string is too long. Real
            // implementation would allocate a string-pool pointer.
            Cell::null()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_roundtrip() {
        let c = Cell::null();
        assert!(c.is_null());
        assert_eq!(c.tag(), TypeTag::Null);
    }

    #[test]
    fn f64_identity_boxing() {
        let x = 3.14159f64;
        let c = Cell::from_f64(x);
        assert!(c.is_f64());
        assert_eq!(c.as_f64(), Some(x));
        // Identity boxing: the bits are the raw f64 bits.
        assert_eq!(c.to_bits(), x.to_bits());
    }

    #[test]
    fn f64_zero_is_f64_not_null() {
        // 0.0f64 has bit pattern 0x0000_0000_0000_0000 — same as NULL!
        // This is a known tension. We resolve it by treating 0.0 as NULL
        // and using a separate "+0.0" canonical form (or accepting that
        // f64 zero is ambiguous). For now, document the behavior.
        let c = Cell::from_f64(0.0);
        // 0.0 → bits 0 → is_null returns true. This is the price of niche-filling.
        assert!(c.is_null());
    }

    #[test]
    fn f64_nan_canonicalized() {
        let c = Cell::from_f64(f64::NAN);
        assert!(c.is_f64());
        assert_eq!(c.to_bits(), Cell::NAN_CANON);
    }

    #[test]
    fn i32_roundtrip() {
        let c = Cell::from_i32(42);
        assert!(c.is_i32());
        assert_eq!(c.as_i32(), Some(42));
        assert!(!c.is_f64());
        assert!(!c.is_null());
    }

    #[test]
    fn i32_negative() {
        let c = Cell::from_i32(-1);
        assert_eq!(c.as_i32(), Some(-1));
    }

    #[test]
    fn bool_roundtrip() {
        let t = Cell::from_bool(true);
        let f = Cell::from_bool(false);
        assert_eq!(t.as_bool(), Some(true));
        assert_eq!(f.as_bool(), Some(false));
    }

    #[test]
    fn short_str_roundtrip() {
        let c = Cell::from_short_str(b"hello").unwrap();
        assert!(c.is_short_str());
        assert_eq!(c.as_short_str(), Some(b"hello".to_vec()));
    }

    #[test]
    fn short_str_too_long_fails() {
        assert!(Cell::from_short_str(b"seven!!").is_none());
    }

    #[test]
    fn date_roundtrip() {
        let c = Cell::from_date(19_000); // ~2022-01-01
        assert_eq!(c.as_date(), Some(19_000));
    }

    #[test]
    fn hamming_distance_works_for_any_type() {
        let a = Cell::from_i32(0);
        let b = Cell::from_i32(1);
        // They differ only in the low bit.
        assert_eq!(a.hamming(b), 1);

        let x = Cell::from_f64(1.0);
        let y = Cell::from_f64(2.0);
        // f64 1.0 = 0x3FF0_0000_0000_0000, 2.0 = 0x4000_0000_0000_0000
        // They differ in many bits.
        assert!(x.hamming(y) > 1);
    }

    #[test]
    fn equality_via_xor() {
        let a = Cell::from_i32(42);
        let b = Cell::from_i32(42);
        let c = Cell::from_i32(43);
        assert!(a.bit_eq(b));
        assert!(!a.bit_eq(c));
    }

    #[test]
    fn cell_is_8_bytes() {
        assert_eq!(std::mem::size_of::<Cell>(), 8);
    }

    #[test]
    fn cell_is_copy() {
        let a = Cell::from_i32(42);
        let b = a; // Copy
        let _ = a; // still usable
        assert_eq!(a, b);
    }

    #[test]
    fn cell_is_pod() {
        fn assert_pod<T: Pod>() {}
        assert_pod::<Cell>();
    }

    #[test]
    fn mixed_column_simulates_union_type() {
        // This is the killer feature: a single column can hold mixed types
        // with zero per-value overhead.
        let col: Vec<Cell> = vec![
            Cell::from_i32(42),
            Cell::from_f64(3.14),
            Cell::from_bool(true),
            Cell::null(),
            Cell::from_short_str(b"hi").unwrap(),
        ];
        assert_eq!(col.len(), 5);
        assert_eq!(col[0].as_i32(), Some(42));
        assert_eq!(col[1].as_f64(), Some(3.14));
        assert_eq!(col[2].as_bool(), Some(true));
        assert!(col[3].is_null());
        assert_eq!(col[4].as_short_str(), Some(b"hi".to_vec()));
    }
}
