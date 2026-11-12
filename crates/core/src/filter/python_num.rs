//! Python 路线算术的数值规则与资源预算（0032）：精确整数、按 CPython 正确舍入的整数转浮点与整数真除法，
//! 以及算术中新分配的值的上限。

use crate::error::{EngineError, Result};
use num_bigint::{BigInt, BigUint, Sign};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::Arc;

/// 单个字符串或列表结果、以及一行内所有新分配的累计上限（按本实现实际分配的字节）。
pub(super) const MAX_FILTER_VALUE_BYTES: u64 = 64 << 20;
/// 大整数的位宽上限，按估算值判断。
pub(super) const MAX_FILTER_INT_BITS: u64 = 1 << 20;
/// 列表每个元素的计费字节：元素是只含 O(1) 克隆形式的共享值，内容共享、不重复计算。
pub(super) const FILTER_LIST_ELEMENT_BYTES: u64 = 64;
/// 字符串按码位存成 `u32`。
pub(super) const FILTER_CODE_POINT_BYTES: u64 = 4;

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub(super) value_bytes: u64,
    pub(super) int_bits: u64,
}
pub(super) const LIMITS: Limits = Limits {
    value_bytes: MAX_FILTER_VALUE_BYTES,
    int_bits: MAX_FILTER_INT_BITS,
};

/// 一行求值中的新分配预算；行与行串行求值，每行开始时清零。
pub(super) struct Budget {
    limits: Limits,
    used: u64,
}
impl Budget {
    pub(super) fn new(limits: Limits) -> Self {
        Self { limits, used: 0 }
    }
    pub(super) fn reset(&mut self) {
        self.used = 0;
    }
    pub(super) fn value_error(&self) -> EngineError {
        EngineError::FilterResourceExhausted(format!(
            "filter value exceeds the {}-byte limit",
            self.limits.value_bytes
        ))
    }
    fn int_error(&self) -> EngineError {
        EngineError::FilterResourceExhausted(format!(
            "filter integer exceeds the {}-bit limit",
            self.limits.int_bits
        ))
    }
    /// 记一次新分配：单值上限与每行累计共用同一个上限，累计不超过即单值不超过。
    pub(super) fn charge(&mut self, bytes: u128) -> Result<()> {
        let total = u128::from(self.used) + bytes;
        if total > u128::from(self.limits.value_bytes) {
            return Err(self.value_error());
        }
        // 不超过 u64 的上限，转换不会截断。
        self.used = total as u64;
        Ok(())
    }
    /// 先按确切容量计费，再 `try_reserve_exact`；分配失败返回同一个资源错误，不 abort。
    pub(super) fn vec<T>(&mut self, len: u128, unit: u64) -> Result<Vec<T>> {
        self.charge(len.saturating_mul(u128::from(unit)))?;
        let len = usize::try_from(len).map_err(|_| self.value_error())?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(len)
            .map_err(|_| self.value_error())?;
        Ok(values)
    }
    fn check_bits(&self, estimate: u64) -> Result<()> {
        if estimate > self.limits.int_bits {
            return Err(self.int_error());
        }
        Ok(())
    }
}

/// Python int：能放进 i64 的用 `Small`，否则共享的大整数（构造处保证不在 i64 范围内）。
#[derive(Debug, Clone)]
pub(crate) enum Integer {
    Small(i64),
    Big(Arc<BigInt>),
}

/// 比较与转换时借用的整数：输入里的十进制文本不必为比较而解析。
#[derive(Clone, Copy)]
pub(super) enum IntRef<'a> {
    Text(&'a str),
    Small(i64),
    Big(&'a Arc<BigInt>),
}

fn text_overflow(_: std::num::ParseIntError) -> EngineError {
    EngineError::invalid("cannot fit 'int' into an index-sized integer")
}
fn float_overflow() -> EngineError {
    EngineError::invalid("int too large to convert to float")
}

/// 十进制文本的有效位数（去掉符号与前导零）。
fn digits(text: &str) -> usize {
    text.trim_start_matches('-').trim_start_matches('0').len()
}

impl Integer {
    fn from_big(value: BigInt) -> Self {
        match i64::try_from(&value) {
            Ok(v) => Self::Small(v),
            Err(_) => Self::Big(Arc::new(value)),
        }
    }
    /// 表达式中的整数字面量：解析器已限制在 4300 位内，编译期解析一次、各行共享，不计费。
    pub(super) fn literal(text: &str) -> Result<Self> {
        if let Ok(v) = text.parse::<i64>() {
            return Ok(Self::Small(v));
        }
        BigInt::parse_bytes(text.as_bytes(), 10)
            .map(Self::from_big)
            .ok_or_else(|| EngineError::Storage("integer literal lost its digits".into()))
    }
    /// 行内输入的十进制文本：解析成大整数之前按 `ceil(位数 × log2 10)` 估算位宽，超限不解析。
    fn parse(text: &str, budget: &mut Budget) -> Result<Self> {
        if let Ok(v) = text.parse::<i64>() {
            return Ok(Self::Small(v));
        }
        // log2(10) 取略大的 3.321928095，估算只会偏大。
        let estimate = (digits(text) as u128 * 3_321_928_095).div_ceil(1_000_000_000);
        budget.check_bits(u64::try_from(estimate).unwrap_or(u64::MAX))?;
        let value = BigInt::parse_bytes(text.as_bytes(), 10)
            .ok_or_else(|| EngineError::Storage("integer value lost its digits".into()))?;
        budget.charge(u128::from(value.bits().div_ceil(8)))?;
        Ok(Self::from_big(value))
    }
    pub(super) fn from_ref(value: IntRef<'_>, budget: &mut Budget) -> Result<Self> {
        Ok(match value {
            IntRef::Text(text) => Self::parse(text, budget)?,
            IntRef::Small(v) => Self::Small(v),
            IntRef::Big(v) => Self::Big(Arc::clone(v)),
        })
    }
    pub(super) fn as_ref(&self) -> IntRef<'_> {
        match self {
            Self::Small(v) => IntRef::Small(*v),
            Self::Big(v) => IntRef::Big(v),
        }
    }
    fn big(&self) -> Cow<'_, BigInt> {
        match self {
            Self::Small(v) => Cow::Owned(BigInt::from(*v)),
            Self::Big(v) => Cow::Borrowed(v),
        }
    }
    fn bits(&self) -> u64 {
        match self {
            Self::Small(v) => u64::from(64 - v.unsigned_abs().leading_zeros()),
            Self::Big(v) => v.bits(),
        }
    }
    /// `+ - *`：小整数溢出时转大整数；大整数先按估算位宽（加减取较大者加一、乘法取两侧之和）检查。
    pub(super) fn apply(op: &str, a: &Self, b: &Self, budget: &mut Budget) -> Result<Self> {
        if let (Self::Small(x), Self::Small(y)) = (a, b) {
            let exact = match op {
                "+" => x.checked_add(*y),
                "-" => x.checked_sub(*y),
                _ => x.checked_mul(*y),
            };
            if let Some(v) = exact {
                return Ok(Self::Small(v));
            }
        }
        let estimate = if op == "*" {
            a.bits().saturating_add(b.bits())
        } else {
            a.bits().max(b.bits()).saturating_add(1)
        };
        budget.check_bits(estimate)?;
        let (x, y) = (a.big(), b.big());
        let value = match op {
            "+" => &*x + &*y,
            "-" => &*x - &*y,
            _ => &*x * &*y,
        };
        let value = Self::from_big(value);
        if let Self::Big(v) = &value {
            budget.charge(u128::from(v.bits().div_ceil(8)))?;
        }
        Ok(value)
    }
    fn is_zero(&self) -> bool {
        match self {
            Self::Small(v) => *v == 0,
            Self::Big(v) => v.sign() == Sign::NoSign,
        }
    }
    fn is_negative(&self) -> bool {
        match self {
            Self::Small(v) => *v < 0,
            Self::Big(v) => v.sign() == Sign::Minus,
        }
    }
    fn magnitude(&self) -> BigUint {
        match self {
            Self::Small(v) => BigUint::from(v.unsigned_abs()),
            Self::Big(v) => v.magnitude().clone(),
        }
    }
}

impl IntRef<'_> {
    /// Python `float(int)`：正确舍入到最近偶数；超出 double 报 OverflowError。文本直接按十进制解析，
    /// Rust 的浮点解析对任意长度都正确舍入，与先得到精确整数再舍入的结果相同，且不必解析成大整数。
    pub(super) fn to_f64(self) -> Result<f64> {
        let value = match self {
            IntRef::Text(text) => text
                .parse::<f64>()
                .map_err(|_| EngineError::Storage("integer value lost its digits".into()))?,
            IntRef::Small(v) => v as f64,
            IntRef::Big(v) => {
                let m = biguint_to_f64(v.magnitude());
                if v.sign() == Sign::Minus { -m } else { m }
            }
        };
        if value.is_finite() {
            // Python int 没有负零：`float(int("-0"))` 是 0.0。
            Ok(if value == 0.0 { 0.0 } else { value })
        } else {
            Err(float_overflow())
        }
    }
    /// 序列重复的次数：须能放进 `Py_ssize_t`，否则 OverflowError（基值为空时也报）。
    pub(super) fn repeat_count(self) -> Result<i64> {
        match self {
            IntRef::Text(text) => text.parse::<i64>().map_err(text_overflow),
            IntRef::Small(v) => Ok(v),
            IntRef::Big(_) => Err(EngineError::invalid(
                "cannot fit 'int' into an index-sized integer",
            )),
        }
    }
}

/// 非负大整数转 double：取最高 64 位，被截掉的低位并成粘滞位，再按 u64 → f64 的最近偶数舍入。
/// 超过 2^1024 时得到 inf，由调用方报 OverflowError。
fn biguint_to_f64(m: &BigUint) -> f64 {
    let bits = m.bits();
    if bits <= 64 {
        return m.iter_u64_digits().next().unwrap_or(0) as f64;
    }
    if bits > 1024 {
        return f64::INFINITY;
    }
    let shift = bits - 64;
    let top = (m >> shift).iter_u64_digits().next().unwrap_or(0);
    let sticky = m.trailing_zeros().is_some_and(|zeros| zeros < shift);
    ldexp((top | u64::from(sticky)) as f64, shift as i64)
}

/// `x × 2^exp`：分段乘，中间结果不溢出、不提前进入次正规。
fn ldexp(mut x: f64, mut exp: i64) -> f64 {
    let pow = |e: i64| f64::from_bits(((e + 1023) as u64) << 52);
    while exp > 1023 {
        x *= pow(1023);
        exp -= 1023;
    }
    while exp < -1022 {
        x *= pow(-1022);
        exp += 1022;
    }
    x * pow(exp)
}

/// Python 的 `int / int`（CPython `long_true_divide`）：除数为 0 时返回 None（ZeroDivisionError，3.x 吞成
/// 未知）；结果正确舍入到最近偶数，含次正规与下溢；超出 double 报 OverflowError。
pub(super) fn true_divide(a: &Integer, b: &Integer) -> Result<Option<f64>> {
    const MANT: i64 = 53;
    const MIN_EXP: i64 = -1021;
    const MAX_EXP: i64 = 1024;
    if b.is_zero() {
        return Ok(None);
    }
    let negate = a.is_negative() != b.is_negative();
    let sign = |v: f64| if negate { -v } else { v };
    let exact = 1u64 << MANT;
    if let (Integer::Small(x), Integer::Small(y)) = (a, b)
        && x.unsigned_abs() <= exact
        && y.unsigned_abs() <= exact
    {
        // 两侧都能精确表示为 double，IEEE 除法本身就是正确舍入。
        return Ok(Some(sign(
            x.unsigned_abs() as f64 / y.unsigned_abs() as f64,
        )));
    }
    let (x, y) = (a.magnitude(), b.magnitude());
    if x.bits() == 0 {
        return Ok(Some(sign(0.0)));
    }
    let diff = x.bits() as i64 - y.bits() as i64;
    if diff > MAX_EXP {
        return Err(EngineError::invalid(
            "integer division result too large for a float",
        ));
    }
    if diff < MIN_EXP - MANT - 1 {
        return Ok(Some(sign(0.0)));
    }
    // 把商移到 MANT+2 或 MANT+3 位（次正规时更少），多出的两三位加上粘滞位决定舍入。
    let shift = diff.max(MIN_EXP) - MANT - 2;
    let (shifted, mut inexact) = if shift <= 0 {
        (&x << (-shift) as u64, false)
    } else {
        let shift = shift as u64;
        (
            &x >> shift,
            x.trailing_zeros().is_some_and(|zeros| zeros < shift),
        )
    };
    let quotient = &shifted / &y;
    inexact |= &quotient * &y != shifted;
    let q_bits = quotient.bits() as i64;
    let mut low = quotient.iter_u64_digits().next().unwrap_or(0);
    let extra = q_bits.max(MIN_EXP - shift) - MANT;
    let mask = 1u64 << (extra - 1);
    low |= u64::from(inexact);
    if low & mask != 0 && low & (3 * mask - 1) != 0 {
        low += mask;
    }
    low &= !(2 * mask - 1);
    let dx = low as f64;
    if shift + q_bits >= MAX_EXP && (shift + q_bits > MAX_EXP || dx == ldexp(1.0, q_bits)) {
        return Err(EngineError::invalid(
            "integer division result too large for a float",
        ));
    }
    Ok(Some(sign(ldexp(dx, shift))))
}

/// 两个 Python int 的大小。输入文本与大整数比较时，位数相差足够大就不必解析；否则解析的文本
/// 不长于另一侧大整数的十进制位数（受位宽上限约束），只用于这一次比较、随即释放。
pub(super) fn int_order(a: IntRef<'_>, b: IntRef<'_>) -> Ordering {
    match (a, b) {
        (IntRef::Text(x), IntRef::Text(y)) => super::python::int_cmp(x, y),
        (IntRef::Text(x), IntRef::Small(y)) => text_small_order(x, y),
        (IntRef::Small(x), IntRef::Text(y)) => text_small_order(y, x).reverse(),
        (IntRef::Small(x), IntRef::Small(y)) => x.cmp(&y),
        (IntRef::Small(x), IntRef::Big(y)) => BigInt::from(x).cmp(&**y),
        (IntRef::Big(x), IntRef::Small(y)) => (**x).cmp(&BigInt::from(y)),
        (IntRef::Big(x), IntRef::Big(y)) => (**x).cmp(&**y),
        (IntRef::Text(x), IntRef::Big(y)) => text_big_order(x, y),
        (IntRef::Big(x), IntRef::Text(y)) => text_big_order(y, x).reverse(),
    }
}

/// 文本放不进 i64 时它的绝对值超出 i64，符号即决定大小；bool 与输入整数的比较走这里，不分配。
fn text_small_order(text: &str, small: i64) -> Ordering {
    match text.parse::<i64>() {
        Ok(value) => value.cmp(&small),
        Err(_) if text.starts_with('-') => Ordering::Less,
        Err(_) => Ordering::Greater,
    }
}

fn text_big_order(text: &str, big: &BigInt) -> Ordering {
    let count = digits(text);
    let text_sign = if count == 0 {
        0
    } else if text.starts_with('-') {
        -1
    } else {
        1
    };
    let big_sign = match big.sign() {
        Sign::Minus => -1,
        Sign::NoSign => 0,
        Sign::Plus => 1,
    };
    if text_sign != big_sign || text_sign == 0 {
        return text_sign.cmp(&big_sign);
    }
    // |text| ∈ [10^(d-1), 10^d)，|big| ∈ [2^(b-1), 2^b)。log2(10) ∈ (3.3219, 3.3220)。
    let (d, b) = (count as u128, u128::from(big.bits()));
    let magnitude = if (d - 1) * 33_219 >= (b + 1) * 10_000 {
        Ordering::Greater
    } else if d * 33_220 + 10_000 <= (b - 1) * 10_000 {
        Ordering::Less
    } else {
        let text = text.trim_start_matches('-');
        match BigUint::parse_bytes(text.as_bytes(), 10) {
            Some(value) => value.cmp(big.magnitude()),
            None => super::python::int_cmp(text, &big.magnitude().to_string()),
        }
    };
    if text_sign < 0 {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Python int 与 float 的大小（NaN 无序）。
pub(super) fn int_float_order(i: IntRef<'_>, float: f64) -> Option<Ordering> {
    match i {
        IntRef::Text(text) => super::python::int_float_cmp(text, float),
        IntRef::Small(v) => {
            if float.is_nan() {
                return None;
            }
            // ±2^63 恰好可表示；区间内 floor 转 i64 精确。
            if float >= 9_223_372_036_854_775_808.0 {
                return Some(Ordering::Less);
            }
            if float < -9_223_372_036_854_775_808.0 {
                return Some(Ordering::Greater);
            }
            let order = v.cmp(&(float.floor() as i64));
            Some(if order.is_eq() && float.fract() != 0.0 {
                Ordering::Less
            } else {
                order
            })
        }
        IntRef::Big(v) => {
            if float.is_nan() {
                return None;
            }
            if float.is_infinite() {
                return Some(if float.is_sign_positive() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                });
            }
            // 大整数在 i64 之外，与它相等的 floor 绝对值至少 2^63，这样的 double 没有小数部分。
            Some((**v).cmp(&float_floor(float)))
        }
    }
}

/// 有限 double 的 floor，精确转成整数。
fn float_floor(float: f64) -> BigInt {
    let floor = float.floor();
    if floor.abs() < 9.0e18 {
        return BigInt::from(floor as i64);
    }
    // |floor| ≥ 2^62：是整数，尾数左移指数位即可精确得到。
    let bits = floor.to_bits();
    let exp = ((bits >> 52) & 0x7ff) - 1075;
    let mantissa = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    let magnitude = BigUint::from(mantissa) << exp;
    BigInt::from_biguint(
        if floor < 0.0 { Sign::Minus } else { Sign::Plus },
        magnitude,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(text: &str) -> Integer {
        Integer::literal(text).unwrap()
    }

    #[test]
    fn biguint_conversion_rounds_half_even_and_overflows_at_2_1024() {
        let two = BigUint::from(2u32);
        // 2^64 + 2^11 恰在 2^64 与 2^64 + 2^12 的中点：偶数一侧是 2^64。
        let m = two.pow(64) + two.pow(11);
        assert_eq!(biguint_to_f64(&m), 2f64.powi(64));
        // 中点加一个更低的位（粘滞位）：向上。
        let m = two.pow(80) + two.pow(27) + BigUint::from(1u32);
        assert_eq!(biguint_to_f64(&m), 2f64.powi(80) + 2f64.powi(28));
        // 2^1024 - 2^970 舍入到 2^1024，溢出。
        let m = two.pow(1024) - two.pow(970);
        assert!(biguint_to_f64(&m).is_infinite());
        let m = two.pow(1024) - two.pow(971);
        assert_eq!(biguint_to_f64(&m), f64::MAX);
        assert_eq!(
            IntRef::Big(&Arc::new(BigInt::from(-1) * BigInt::from(two.pow(1024))))
                .to_f64()
                .unwrap_err()
                .to_string(),
            "int too large to convert to float"
        );
    }

    #[test]
    fn true_division_matches_cpython() {
        // 期望值由 CPython 3.12 计算。
        let cases: [(&str, &str, f64); 8] = [
            ("1", "3", 0.333_333_333_333_333_3),
            ("10", "4", 2.5),
            ("0", "-5", -0.0),
            ("-7", "2", -3.5),
            ("9007199254740993", "1", 9_007_199_254_740_992.0),
            ("9007199254740995", "1", 9_007_199_254_740_996.0),
            ("18014398509481983", "2", 9_007_199_254_740_992.0),
            ("-18014398509481985", "2", -9_007_199_254_740_992.0),
        ];
        for (a, b, expected) in cases {
            let actual = true_divide(&big(a), &big(b)).unwrap().unwrap();
            assert_eq!(actual.to_bits(), expected.to_bits(), "{a} / {b}");
        }
        let one = big("1");
        let huge = Integer::from_big(BigInt::from(1) << 1074u32);
        // 1 / 2^1074 是最小次正规数；1 / 2^1075 恰为它的一半，舍入到偶数 0。
        assert_eq!(
            true_divide(&one, &huge).unwrap().unwrap(),
            f64::from_bits(1)
        );
        let huge = Integer::from_big(BigInt::from(1) << 1075u32);
        assert_eq!(true_divide(&one, &huge).unwrap().unwrap(), 0.0);
        let huge = Integer::from_big((BigInt::from(1) << 1075u32) - 1);
        assert_eq!(
            true_divide(&one, &huge).unwrap().unwrap(),
            f64::from_bits(1)
        );
        let huge = Integer::from_big(BigInt::from(1) << 1024u32);
        assert_eq!(
            true_divide(&huge, &one).unwrap_err().to_string(),
            "integer division result too large for a float"
        );
        assert_eq!(
            true_divide(&huge, &big("2")).unwrap().unwrap(),
            2f64.powi(1023)
        );
        assert!(true_divide(&huge, &big("0")).unwrap().is_none());
    }

    #[test]
    fn integer_order_avoids_parsing_far_apart_text() {
        let big = Arc::new(BigInt::from(1) << 200u32);
        let wide = format!("1{}", "0".repeat(5000));
        assert_eq!(
            int_order(IntRef::Text(&wide), IntRef::Big(&big)),
            Ordering::Greater
        );
        assert_eq!(
            int_order(IntRef::Text("-5"), IntRef::Big(&big)),
            Ordering::Less
        );
        let exact = big.to_string();
        assert_eq!(
            int_order(IntRef::Text(&exact), IntRef::Big(&big)),
            Ordering::Equal
        );
        assert_eq!(
            int_float_order(IntRef::Big(&big), 2f64.powi(200)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            int_float_order(IntRef::Big(&big), 2f64.powi(200) + 2f64.powi(160)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn general_division_path_matches_ieee_on_exact_operands() {
        // 两侧同乘 2^60 迫使走大整数路径；商不变，期望值就是 IEEE 对精确小整数的除法。
        for x in -60i64..=60 {
            for y in (-40i64..=40).filter(|y| *y != 0) {
                let scale = |v: i64| Integer::from_big(BigInt::from(v) << 60u32);
                let actual = true_divide(&scale(x), &scale(y)).unwrap().unwrap();
                let expected = x as f64 / y as f64;
                assert_eq!(actual.to_bits(), expected.to_bits(), "{x} / {y}");
            }
        }
        let one = big("1");
        let pow = |e: u32| BigInt::from(1) << e;
        // 2^1024 - 2^970 在 f64::MAX 与 2^1024 的中点，舍入到偶数一侧的 2^1024，溢出。
        assert!(true_divide(&Integer::from_big(pow(1024) - pow(970)), &one).is_err());
        assert_eq!(
            true_divide(&Integer::from_big(pow(1024) - pow(971)), &one)
                .unwrap()
                .unwrap(),
            f64::MAX
        );
        // 3 / 2^1075 = 1.5 个最小次正规数，舍入到偶数的 2 个。
        assert_eq!(
            true_divide(&big("3"), &Integer::from_big(pow(1075)))
                .unwrap()
                .unwrap(),
            f64::from_bits(2)
        );
        assert_eq!(
            true_divide(&big("-3"), &Integer::from_big(pow(1075)))
                .unwrap()
                .unwrap(),
            -f64::from_bits(2)
        );
    }

    #[test]
    fn text_and_small_integer_orders_match_exact_comparison() {
        for bits in [64u32, 65, 100, 333, 1000] {
            for negative in [false, true] {
                let mut magnitude = BigInt::from(1) << (bits - 1);
                magnitude += BigInt::from(12_345);
                let big = Arc::new(if negative { -magnitude } else { magnitude });
                let digits = big.magnitude().to_string().len();
                for d in digits.saturating_sub(3).max(1)..=digits + 3 {
                    let ten = BigInt::from(10).pow(d as u32);
                    for value in [ten.clone() / 10, ten.clone() - 1, ten / 10 + 7] {
                        for sign in [1i32, -1] {
                            let value: BigInt = &value * BigInt::from(sign);
                            let text = value.to_string();
                            assert_eq!(
                                int_order(IntRef::Text(&text), IntRef::Big(&big)),
                                value.cmp(&big),
                                "{text} vs {big}"
                            );
                        }
                    }
                }
                let exact = big.to_string();
                assert_eq!(
                    int_order(IntRef::Big(&big), IntRef::Text(&exact)),
                    Ordering::Equal
                );
            }
        }
        let wide = "9".repeat(20);
        let negative = format!("-{wide}");
        assert_eq!(
            int_order(IntRef::Text(&wide), IntRef::Small(i64::MAX)),
            Ordering::Greater
        );
        assert_eq!(
            int_order(IntRef::Text(&negative), IntRef::Small(i64::MIN)),
            Ordering::Less
        );
        assert_eq!(
            int_order(IntRef::Small(1), IntRef::Text("-0")),
            Ordering::Greater
        );
        let two63 = 9_223_372_036_854_775_808.0;
        for (v, f, expected) in [
            (i64::MAX, two63, Some(Ordering::Less)),
            (i64::MIN, -two63, Some(Ordering::Equal)),
            (i64::MIN, -two63 * 2.0, Some(Ordering::Greater)),
            (3, 3.5, Some(Ordering::Less)),
            (3, 2.5, Some(Ordering::Greater)),
            (-3, -3.0, Some(Ordering::Equal)),
            (-3, -2.5, Some(Ordering::Less)),
            (1, f64::INFINITY, Some(Ordering::Less)),
            (1, f64::NEG_INFINITY, Some(Ordering::Greater)),
            (1, f64::NAN, None),
        ] {
            assert_eq!(int_float_order(IntRef::Small(v), f), expected, "{v} vs {f}");
        }
    }

    #[test]
    fn budget_checks_estimates_before_allocating() {
        let mut budget = Budget::new(Limits {
            value_bytes: 100,
            int_bits: 64,
        });
        assert!(budget.vec::<u32>(25, 4).is_ok());
        assert_eq!(
            budget.vec::<u32>(1, 4).unwrap_err().to_string(),
            "filter value exceeds the 100-byte limit"
        );
        budget.reset();
        // 20 位十进制估算为 67 位，超过 64 位的上限；19 位估算 64 位，放行。
        assert_eq!(
            Integer::parse("99999999999999999999", &mut budget)
                .unwrap_err()
                .to_string(),
            "filter integer exceeds the 64-bit limit"
        );
        assert!(Integer::parse("-9223372036854775808", &mut budget).is_ok());
        // 分配器给不出的容量返回同一个资源错误，不 abort。
        let mut budget = Budget::new(Limits {
            value_bytes: u64::MAX,
            int_bits: 64,
        });
        assert_eq!(
            budget.vec::<u32>(1 << 61, 4).unwrap_err().to_string(),
            format!("filter value exceeds the {}-byte limit", u64::MAX)
        );
    }
}
