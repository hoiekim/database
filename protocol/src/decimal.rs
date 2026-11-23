//! Exact decimal numbers: an integer and how many of its digits follow the
//! point. Sums of money come out to the cent, which floats do not promise.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// The most digits kept after the point.
pub const MAX_SCALE: u8 = 30;
/// The most digits of a number read from text that are kept in all.
const MAX_DIGITS: usize = 37;

#[derive(Clone, Copy, Debug)]
pub struct Decimal {
    mantissa: i128,
    scale: u8,
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

impl Decimal {
    pub const ZERO: Decimal = Decimal { mantissa: 0, scale: 0 };

    /// `mantissa / 10^scale`.
    pub fn new(mantissa: i128, scale: u8) -> Decimal {
        Decimal { mantissa, scale }
    }

    pub fn mantissa(&self) -> i128 {
        self.mantissa
    }

    pub fn scale(&self) -> u8 {
        self.scale
    }

    pub fn is_zero(&self) -> bool {
        self.mantissa == 0
    }

    pub fn is_negative(&self) -> bool {
        self.mantissa < 0
    }

    /// The same number written with `scale` digits after the point, or
    /// `None` if that takes more than 128 bits.
    fn widened(&self, scale: u8) -> Option<i128> {
        self.mantissa.checked_mul(pow10(u32::from(scale - self.scale))?)
    }

    fn aligned(&self, other: &Decimal) -> Option<(i128, i128, u8)> {
        let scale = self.scale.max(other.scale);
        Some((self.widened(scale)?, other.widened(scale)?, scale))
    }

    pub fn checked_add(&self, other: &Decimal) -> Option<Decimal> {
        let (a, b, scale) = self.aligned(other)?;
        Some(Decimal { mantissa: a.checked_add(b)?, scale })
    }

    pub fn checked_sub(&self, other: &Decimal) -> Option<Decimal> {
        let (a, b, scale) = self.aligned(other)?;
        Some(Decimal { mantissa: a.checked_sub(b)?, scale })
    }

    pub fn checked_mul(&self, other: &Decimal) -> Option<Decimal> {
        let product = Decimal { mantissa: self.mantissa.checked_mul(other.mantissa)?, scale: self.scale + other.scale };
        Some(if product.scale > MAX_SCALE { product.round(MAX_SCALE) } else { product })
    }

    /// Division, to at least twelve digits after the point. `None` when
    /// dividing by zero or when the result does not fit.
    pub fn checked_div(&self, other: &Decimal) -> Option<Decimal> {
        if other.mantissa == 0 {
            return None;
        }
        let scale = (self.scale.max(other.scale) + 12).min(MAX_SCALE);
        // a/10^sa ÷ b/10^sb = (a × 10^(scale + sb − sa)) / b, over 10^scale.
        let shift = i32::from(scale) + i32::from(other.scale) - i32::from(self.scale);
        let numerator = self.mantissa.checked_mul(pow10(shift as u32)?)?;
        // Checked: the least 128-bit number divided by -1 does not fit.
        let (q, r) = (numerator.checked_div(other.mantissa)?, numerator.checked_rem(other.mantissa)?);
        // Round half away from zero.
        let round_up = r.unsigned_abs() * 2 >= other.mantissa.unsigned_abs();
        let sign = if (numerator < 0) != (other.mantissa < 0) { -1 } else { 1 };
        Some(Decimal { mantissa: if round_up { q + sign } else { q }, scale }.trimmed())
    }

    /// `None` for the one number whose negation does not fit.
    pub fn checked_neg(&self) -> Option<Decimal> {
        Some(Decimal { mantissa: self.mantissa.checked_neg()?, scale: self.scale })
    }

    pub fn checked_abs(&self) -> Option<Decimal> {
        Some(Decimal { mantissa: self.mantissa.checked_abs()?, scale: self.scale })
    }

    /// The negation, or the largest number if it does not fit.
    pub fn neg(&self) -> Decimal {
        self.checked_neg().unwrap_or(Decimal { mantissa: i128::MAX, scale: self.scale })
    }

    pub fn abs(&self) -> Decimal {
        self.checked_abs().unwrap_or(Decimal { mantissa: i128::MAX, scale: self.scale })
    }

    /// Rounds to `scale` digits after the point, halves away from zero.
    pub fn round(&self, scale: u8) -> Decimal {
        if scale >= self.scale {
            return *self;
        }
        let Some(divisor) = pow10(u32::from(self.scale - scale)) else { return Decimal { mantissa: 0, scale } };
        let (q, r) = (self.mantissa / divisor, self.mantissa % divisor);
        let mantissa = if r.abs() * 2 >= divisor { q + self.mantissa.signum() } else { q };
        Decimal { mantissa, scale }
    }

    /// The same number with exactly `scale` digits after the point: rounded
    /// if it has more, padded with zeros if fewer and they fit.
    pub fn rescaled(&self, scale: u8) -> Decimal {
        if scale <= self.scale {
            return self.round(scale);
        }
        match self.widened(scale) {
            Some(mantissa) => Decimal { mantissa, scale },
            None => *self,
        }
    }

    /// Drops zeros at the end of the fraction: 1.500 becomes 1.5.
    pub fn trimmed(&self) -> Decimal {
        let mut d = *self;
        while d.scale > 0 && d.mantissa % 10 == 0 {
            d.mantissa /= 10;
            d.scale -= 1;
        }
        d
    }

    pub fn to_f64(&self) -> f64 {
        // Through text: dividing two floats would round twice.
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// A float as a decimal, the way Postgres converts `float8` to
    /// `numeric`: through its first 15 significant digits, so 0.1 gives
    /// exactly 0.1 and the noise beyond a double's precision is dropped.
    /// `None` for NaN, infinity, and numbers too large for 128 bits.
    pub fn from_f64(f: f64) -> Option<Decimal> {
        if !f.is_finite() {
            return None;
        }
        format!("{f:.14e}").parse::<Decimal>().ok().map(|d| d.trimmed())
    }

    /// The whole part, toward zero, if it fits.
    pub fn to_i64(&self) -> Option<i64> {
        i64::try_from(self.mantissa / pow10(u32::from(self.scale))?).ok()
    }
}

impl From<i64> for Decimal {
    fn from(n: i64) -> Self {
        Decimal { mantissa: n.into(), scale: 0 }
    }
}

impl From<i32> for Decimal {
    fn from(n: i32) -> Self {
        Decimal { mantissa: n.into(), scale: 0 }
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.aligned(other) {
            Some((a, b, _)) => a.cmp(&b),
            // Too wide to align: only possible when the magnitudes are far
            // apart, so the sign and the whole part decide.
            None => {
                let (a, b) = (self.trimmed(), other.trimmed());
                match a.mantissa.signum().cmp(&b.mantissa.signum()) {
                    Ordering::Equal => {
                        let whole = |d: &Decimal| d.mantissa / pow10(u32::from(d.scale)).unwrap_or(i128::MAX);
                        whole(&a).cmp(&whole(&b)).then_with(|| a.to_f64().total_cmp(&b.to_f64()))
                    }
                    o => o,
                }
            }
        }
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = usize::from(self.scale);
        let sign = if self.mantissa < 0 { "-" } else { "" };
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = digits.split_at(digits.len() - scale);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseDecimalError;

impl fmt::Display for ParseDecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a decimal number")
    }
}

impl std::error::Error for ParseDecimalError {}

impl FromStr for Decimal {
    type Err = ParseDecimalError;

    /// Reads `-12.34`, `.5`, `1e3`, `2.5E-4`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (number, exponent) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i32>().map_err(|_| ParseDecimalError)?),
            None => (s, 0),
        };
        let (negative, number) = match number.as_bytes().first() {
            Some(b'-') => (true, &number[1..]),
            Some(b'+') => (false, &number[1..]),
            _ => (false, number),
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        if whole.is_empty() && fraction.is_empty() {
            return Err(ParseDecimalError);
        }
        if !whole.bytes().chain(fraction.bytes()).all(|c| c.is_ascii_digit()) {
            return Err(ParseDecimalError);
        }
        // More digits after the point than can be kept are rounded away,
        // halves away from zero, rather than refused: only a whole part
        // too large is out of range.
        let significant = whole.trim_start_matches('0').len();
        let room = (MAX_DIGITS.saturating_sub(significant)).min(usize::from(MAX_SCALE));
        let (fraction, round_up) = match fraction.as_bytes().get(room) {
            Some(next) => (&fraction[..room], *next >= b'5'),
            None => (fraction, false),
        };
        let mut mantissa: i128 = 0;
        for c in whole.bytes().chain(fraction.bytes()) {
            mantissa = mantissa.checked_mul(10).and_then(|m| m.checked_add(i128::from(c - b'0'))).ok_or(ParseDecimalError)?;
        }
        if round_up {
            mantissa = mantissa.checked_add(1).ok_or(ParseDecimalError)?;
        }
        let mut scale = fraction.len() as i32 - exponent;
        if scale < 0 {
            mantissa = pow10((-scale) as u32).and_then(|p| mantissa.checked_mul(p)).ok_or(ParseDecimalError)?;
            scale = 0;
        }
        let d = Decimal { mantissa: if negative { -mantissa } else { mantissa }, scale: u8::try_from(scale).map_err(|_| ParseDecimalError)? };
        Ok(if d.scale > MAX_SCALE { d.round(MAX_SCALE) } else { d })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_prints() {
        for s in ["0", "1", "-1", "12.34", "-0.05", "0.000001", "123456789012345678.90"] {
            assert_eq!(d(s).to_string(), s);
        }
        assert_eq!(d(".5").to_string(), "0.5");
        assert_eq!(d("1e3").to_string(), "1000");
        assert_eq!(d("2.5E-4").to_string(), "0.00025");
        assert_eq!(d("+7").to_string(), "7");
        for bad in ["", ".", "-", "1.2.3", "abc", "1e", "0x10"] {
            assert!(bad.parse::<Decimal>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_edges_of_128_bits_are_errors_not_panics() {
        let least = Decimal::new(i128::MIN, 0);
        assert!(least.checked_div(&d("-1")).is_none());
        assert!(least.checked_neg().is_none() && least.checked_abs().is_none());
        assert_eq!(least.neg().mantissa(), i128::MAX);
        assert!(Decimal::new(i128::MAX, 0).checked_add(&d("1")).is_none());
        assert!(d("1").checked_div(&d("0")).is_none());
    }

    #[test]
    fn digits_beyond_what_fits_are_rounded_away() {
        // Forty digits after the point: kept to thirty, rounded.
        assert_eq!("0.1234567890123456789012345678905555555555".parse::<Decimal>().unwrap().to_string(), "0.123456789012345678901234567891");
        assert_eq!("-2.00000000000000000000000000000049".parse::<Decimal>().unwrap(), d("-2"));
        // A long whole part leaves less room after the point.
        let long: Decimal = "12345678901234567890123456789012345.123456".parse().unwrap();
        assert_eq!(long.to_string(), "12345678901234567890123456789012345.12");
        // A whole part too long for 128 bits is still refused.
        assert!("1234567890123456789012345678901234567890".parse::<Decimal>().is_err());
    }

    #[test]
    fn arithmetic_is_exact() {
        assert_eq!(d("0.1").checked_add(&d("0.2")).unwrap(), d("0.3"));
        assert_eq!(d("10").checked_sub(&d("0.01")).unwrap().to_string(), "9.99");
        assert_eq!(d("1.5").checked_mul(&d("-2.25")).unwrap().to_string(), "-3.375");
        assert_eq!(d("1").checked_div(&d("3")).unwrap().to_string(), "0.333333333333");
        assert_eq!(d("2").checked_div(&d("3")).unwrap().to_string(), "0.666666666667");
        assert_eq!(d("-2").checked_div(&d("3")).unwrap().to_string(), "-0.666666666667");
        assert_eq!(d("10").checked_div(&d("4")).unwrap().to_string(), "2.5");
        assert!(d("1").checked_div(&d("0")).is_none());
        let mut total = Decimal::ZERO;
        for _ in 0..1000 {
            total = total.checked_add(&d("0.01")).unwrap();
        }
        assert_eq!(total, d("10"));
    }

    #[test]
    fn compares_by_value_whatever_the_scale() {
        assert_eq!(d("1.50"), d("1.5"));
        assert!(d("-1.5") < d("-1.25"));
        assert!(d("2") > d("1.999999999999"));
        assert!(d("99999999999999999999999999999") > d("0.000000000000000000000000001"));
        assert!(d("-99999999999999999999999999999") < d("0.000000000000000000000000001"));
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(d("2.345").round(2).to_string(), "2.35");
        assert_eq!(d("-2.345").round(2).to_string(), "-2.35");
        assert_eq!(d("2.344").round(2).to_string(), "2.34");
        assert_eq!(d("1.500").trimmed().to_string(), "1.5");
        assert_eq!(d("12.99").to_i64(), Some(12));
    }

    #[test]
    fn floats_convert_as_they_print() {
        assert_eq!(Decimal::from_f64(0.1).unwrap(), d("0.1"));
        assert_eq!(Decimal::from_f64(-12.5).unwrap().to_string(), "-12.5");
        assert_eq!(Decimal::from_f64(0.1 + 0.2).unwrap().to_string(), "0.3");
        assert_eq!(Decimal::from_f64(2.675).unwrap().round(2).to_string(), "2.68");
        assert_eq!(Decimal::from_f64(1e20).unwrap().to_string(), "100000000000000000000");
        assert_eq!(Decimal::from_f64(0.0).unwrap().to_string(), "0");
        assert!(Decimal::from_f64(f64::NAN).is_none());
        assert_eq!(d("0.1").to_f64(), 0.1);
    }
}
