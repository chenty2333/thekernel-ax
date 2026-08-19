//! Checked integer EEVDF model.
//!
//! The model owns no task reference, intrusive link, tree, queue ownership,
//! real-time class, or task-layer state.  It is deliberately a small value
//! layer which a scheduler can embed in its own allocation-free entity.

use crate::eevdf_profile::EEVDF_PROFILE;

pub const FP_SHIFT: u32 = 32;
pub const ONE: u128 = 1u128 << FP_SHIFT;
pub const NICE_0: u128 = 1024;
/// Work represented by one real tick at nice zero.
pub const WORK: u128 = NICE_0 * ONE;

/// Profile-selected normal-class request target.  The balanced value remains
/// eight ticks, matching the pre-profile model.
pub const TARGET_TICKS_NORMAL: u128 = EEVDF_PROFILE.normal_target_ticks;
/// Profile-selected batch-class request target.  The balanced value remains
/// thirty-two ticks, matching the pre-profile model.
pub const TARGET_TICKS_BATCH: u128 = EEVDF_PROFILE.batch_target_ticks;
/// Profile-selected idle-class request target.  The balanced value remains
/// eight ticks, matching the pre-profile model.
pub const TARGET_TICKS_IDLE: u128 = EEVDF_PROFILE.idle_target_ticks;
/// Profile-selected sleeper grace in fixed-point virtual-time units.
pub const GRACE: i128 = (EEVDF_PROFILE.sleeper_grace_ticks * ONE) as i128;
/// Profile-selected sleeper decay window in fixed-point virtual-time units.
pub const DECAY_WINDOW: i128 = (EEVDF_PROFILE.sleeper_decay_ticks * ONE) as i128;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestClass {
    Normal,
    Batch,
    Idle,
}

impl RequestClass {
    pub const fn target_ticks(self) -> u128 {
        match self {
            Self::Normal => TARGET_TICKS_NORMAL,
            Self::Batch => TARGET_TICKS_BATCH,
            Self::Idle => TARGET_TICKS_IDLE,
        }
    }

    pub const fn target_q(self) -> u128 {
        self.target_ticks()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    ArithmeticExhausted,
    InvalidWeight,
    InvalidState,
}

pub const fn checked_i128(value: u128) -> Result<i128, ModelError> {
    if value > i128::MAX as u128 {
        Err(ModelError::ArithmeticExhausted)
    } else {
        Ok(value as i128)
    }
}

pub const fn checked_mul_i128_u128(value: i128, factor: u128) -> Result<i128, ModelError> {
    if value == 0 {
        return Ok(0);
    }
    let magnitude = value.unsigned_abs();
    let product = match magnitude.checked_mul(factor) {
        Some(product) => product,
        None => return Err(ModelError::ArithmeticExhausted),
    };
    if value < 0 {
        if product > (1u128 << 127) {
            return Err(ModelError::ArithmeticExhausted);
        }
        if product == (1u128 << 127) {
            Ok(i128::MIN)
        } else {
            Ok(-(product as i128))
        }
    } else {
        if product > i128::MAX as u128 {
            return Err(ModelError::ArithmeticExhausted);
        }
        Ok(product as i128)
    }
}

pub const fn checked_ceil_div(numerator: u128, denominator: u128) -> Result<u128, ModelError> {
    if denominator == 0 {
        return Err(ModelError::InvalidWeight);
    }
    let quotient = numerator / denominator;
    if numerator % denominator == 0 {
        Ok(quotient)
    } else {
        match quotient.checked_add(1) {
            Some(value) => Ok(value),
            None => Err(ModelError::ArithmeticExhausted),
        }
    }
}

/// Divide a 129-bit unsigned integer (`high * 2^128 + low`) by a non-zero
/// u128 denominator.  The quotient must fit in u128; a set quotient bit at
/// position 128 is reported as arithmetic exhaustion.  The loop is over the
/// fixed width of the representation, never over an input value.
fn checked_div_129(high: bool, low: u128, denominator: u128) -> Result<(u128, u128), ModelError> {
    if denominator == 0 {
        return Err(ModelError::InvalidWeight);
    }
    let half = denominator >> 1;
    let odd = denominator & 1 != 0;
    let mut quotient = 0u128;
    let mut remainder = 0u128;

    for position in (0..=128).rev() {
        let bit = if position == 128 {
            high
        } else {
            (low >> position) & 1 != 0
        };

        // Compute 2*remainder + bit modulo denominator without ever forming
        // a 129-bit temporary.  `remainder < denominator` is maintained.
        let (quotient_bit, next_remainder) = if !odd {
            if remainder >= half {
                (true, (remainder - half) * 2 + u128::from(bit))
            } else {
                (false, remainder * 2 + u128::from(bit))
            }
        } else if remainder > half {
            // For d = 2*half + 1, 2*remainder + bit - d is
            // 2*(remainder - half) - 1 + bit.
            (true, (remainder - half) * 2 - 1 + u128::from(bit))
        } else if remainder == half && bit {
            (true, 0)
        } else {
            (false, remainder * 2 + u128::from(bit))
        };
        remainder = next_remainder;

        if position == 128 {
            // The first quotient bit represents 2^128, which cannot be
            // represented by the return type.
            if quotient_bit {
                return Err(ModelError::ArithmeticExhausted);
            }
        } else {
            if quotient > (u128::MAX >> 1) {
                return Err(ModelError::ArithmeticExhausted);
            }
            quotient = (quotient << 1) | u128::from(quotient_bit);
        }
    }
    Ok((quotient, remainder))
}

/// Compute the quotient and remainder of `a*b/denominator` without forming
/// an overflowing product.  Ordinary scheduler operands use the native
/// checked product and are O(1).  If that product overflows, the fallback is
/// fixed-width binary multiply/divide: 128 iterations each perform a
/// constant-width 129-bit division, i.e. O(word_bits^2), independent of the
/// numerical magnitude of any input.  Both paths report quotient overflow
/// exactly.
fn checked_mul_div_rem(a: u128, b: u128, denominator: u128) -> Result<(u128, u128), ModelError> {
    if denominator == 0 {
        return Err(ModelError::InvalidWeight);
    }
    // Request quanta, virtual lengths, and normal tick work all fit this
    // product.  Keep their hot path out of the fixed-width fallback.
    if let Some(product) = a.checked_mul(b) {
        return Ok((product / denominator, product % denominator));
    }
    let mut quotient = 0u128;
    let mut remainder = 0u128;
    for position in (0..128).rev() {
        let (double_quotient, doubled_remainder) =
            checked_div_129(remainder >> 127 != 0, remainder << 1, denominator)?;
        let (add_quotient, next_remainder) = if (a >> position) & 1 != 0 {
            let (low, high) = doubled_remainder.overflowing_add(b);
            checked_div_129(high, low, denominator)?
        } else {
            (0, doubled_remainder)
        };
        let extra = double_quotient
            .checked_add(add_quotient)
            .ok_or(ModelError::ArithmeticExhausted)?;
        quotient = quotient
            .checked_mul(2)
            .and_then(|value| value.checked_add(extra))
            .ok_or(ModelError::ArithmeticExhausted)?;
        remainder = next_remainder;
    }
    Ok((quotient, remainder))
}

/// Compute `ceil(a*b/denominator)` without forming an overflowing product.
fn checked_mul_div_ceil(a: u128, b: u128, denominator: u128) -> Result<u128, ModelError> {
    let (quotient, remainder) = checked_mul_div_rem(a, b, denominator)?;
    if remainder == 0 {
        Ok(quotient)
    } else {
        quotient
            .checked_add(1)
            .ok_or(ModelError::ArithmeticExhausted)
    }
}

/// Greatest common divisor over the fixed-width integer domain.  The loop is
/// bounded by the width of `u128`, so a malformed scheduler value cannot turn
/// a model operation into a value-proportional walk.
fn gcd_u128(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// A fixed-width 256-bit unsigned value used only when a rational operation
/// has to stage an intermediate wider than the scheduler's public u128
/// representation.  The final quotient is still required to fit in u128;
/// this type prevents a reducible intermediate from being mistaken for real
/// arithmetic exhaustion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Wide {
    limbs: [u64; 4],
}

impl Wide {
    const ZERO: Self = Self { limbs: [0; 4] };

    fn cmp(self, other: Self) -> core::cmp::Ordering {
        for index in (0..4).rev() {
            match self.limbs[index].cmp(&other.limbs[index]) {
                core::cmp::Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        core::cmp::Ordering::Equal
    }

    fn ge(self, other: Self) -> bool {
        self.cmp(other) != core::cmp::Ordering::Less
    }

    fn bit(self, position: usize) -> bool {
        (self.limbs[position / 64] >> (position % 64)) & 1 != 0
    }

    fn shl1(self) -> (bool, Self) {
        let mut limbs = [0u64; 4];
        let mut carry = false;
        for (index, limb) in limbs.iter_mut().enumerate() {
            let value = self.limbs[index];
            *limb = (value << 1) | u64::from(carry);
            carry = value >> 63 != 0;
        }
        (carry, Self { limbs })
    }

    fn wrapping_sub(self, other: Self) -> Self {
        let mut limbs = [0u64; 4];
        let mut borrow = false;
        for (index, limb) in limbs.iter_mut().enumerate() {
            let (value, first_borrow) = self.limbs[index].overflowing_sub(other.limbs[index]);
            let (value, second_borrow) = value.overflowing_sub(u64::from(borrow));
            *limb = value;
            borrow = first_borrow || second_borrow;
        }
        Self { limbs }
    }

    fn checked_add_one(self) -> Option<Self> {
        let mut next = self;
        for limb in &mut next.limbs {
            let (value, carry) = limb.overflowing_add(1);
            *limb = value;
            if !carry {
                return Some(next);
            }
        }
        None
    }

    fn set_low_bit(&mut self, bit: bool) {
        self.limbs[0] = (self.limbs[0] & !1) | u64::from(bit);
    }

    fn mul_u128(left: u128, right: u128) -> Option<Self> {
        let left = [left as u64, (left >> 64) as u64];
        let right = [right as u64, (right >> 64) as u64];
        let mut limbs = [0u64; 4];

        for (left_index, &left_limb) in left.iter().enumerate() {
            let mut carry = 0u128;
            for (right_index, &right_limb) in right.iter().enumerate() {
                let index = left_index + right_index;
                let value = (left_limb as u128)
                    .checked_mul(right_limb as u128)?
                    .checked_add(limbs[index] as u128)?
                    .checked_add(carry)?;
                limbs[index] = value as u64;
                carry = value >> 64;
            }
            let mut index = left_index + 2;
            while carry != 0 {
                if index == limbs.len() {
                    return None;
                }
                let (value, overflow) = limbs[index].overflowing_add(carry as u64);
                limbs[index] = value;
                carry = (carry >> 64) + u128::from(overflow);
                index += 1;
            }
        }
        Some(Self { limbs })
    }

    fn mul(self, right: u128) -> Option<Self> {
        let mut result = Self::ZERO;
        for index in 0..4 {
            let product = (self.limbs[index] as u128)
                .checked_mul(right as u64 as u128)?
                .checked_add(result.limbs[index] as u128)?;
            result.limbs[index] = product as u64;
            let mut carry = product >> 64;
            let high_product = (self.limbs[index] as u128).checked_mul(right >> 64)?;
            // This helper is intentionally limited to multiplying a value
            // which is already known to fit in 256 bits by a scheduler-sized
            // factor.  Fold the high half at the next limb and reject any
            // carry past the fixed width.
            if index + 1 >= 4 && (high_product != 0 || carry != 0) {
                return None;
            }
            if index + 1 < 4 {
                let next = high_product
                    .checked_add(result.limbs[index + 1] as u128)?
                    .checked_add(carry)?;
                result.limbs[index + 1] = next as u64;
                carry = next >> 64;
                let mut carry_index = index + 2;
                while carry != 0 {
                    if carry_index == 4 {
                        return None;
                    }
                    let (value, overflow) = result.limbs[carry_index].overflowing_add(carry as u64);
                    result.limbs[carry_index] = value;
                    carry = (carry >> 64) + u128::from(overflow);
                    carry_index += 1;
                }
            }
        }
        Some(result)
    }

    fn div_exact_u128(self, divisor: u128) -> Result<Self, ModelError> {
        if divisor == 0 {
            return Err(ModelError::InvalidWeight);
        }
        let mut quotient = Self::ZERO;
        let mut remainder = 0u128;
        for position in (0..256).rev() {
            let (quotient_bit, next_remainder) = checked_div_129(
                remainder >> 127 != 0,
                (remainder << 1) | u128::from(self.bit(position)),
                divisor,
            )?;
            if quotient_bit > 1 {
                return Err(ModelError::InvalidState);
            }
            quotient = quotient.shl1().1;
            quotient.set_low_bit(quotient_bit != 0);
            remainder = next_remainder;
        }
        if remainder == 0 {
            Ok(quotient)
        } else {
            Err(ModelError::InvalidState)
        }
    }

    fn rem_u128(self, divisor: u128) -> Result<u128, ModelError> {
        if divisor == 0 {
            return Err(ModelError::InvalidWeight);
        }
        let mut remainder = 0u128;
        for position in (0..256).rev() {
            let (_, next_remainder) = checked_div_129(
                remainder >> 127 != 0,
                (remainder << 1) | u128::from(self.bit(position)),
                divisor,
            )?;
            remainder = next_remainder;
        }
        Ok(remainder)
    }

    fn low_u128(self) -> Result<u128, ModelError> {
        if self.limbs[2] != 0 || self.limbs[3] != 0 {
            return Err(ModelError::ArithmeticExhausted);
        }
        Ok((self.limbs[1] as u128) << 64 | self.limbs[0] as u128)
    }
}

fn checked_ratio_ceil(
    mut numerator_factors: [u128; 3],
    mut denominator_factors: [u128; 2],
) -> Result<u128, ModelError> {
    for numerator in &mut numerator_factors {
        for denominator in &mut denominator_factors {
            let gcd = gcd_u128(*numerator, *denominator);
            *numerator /= gcd;
            *denominator /= gcd;
        }
    }
    let numerator = numerator_factors
        .into_iter()
        .try_fold(1u128, |value, factor| value.checked_mul(factor));
    let denominator = denominator_factors
        .into_iter()
        .try_fold(1u128, |value, factor| value.checked_mul(factor));
    if let (Some(numerator), Some(denominator)) = (numerator, denominator) {
        return checked_ceil_div(numerator, denominator);
    }

    let numerator = Wide::mul_u128(numerator_factors[0], numerator_factors[1])
        .and_then(|value| value.mul(numerator_factors[2]))
        .ok_or(ModelError::ArithmeticExhausted)?;
    let denominator = Wide::mul_u128(denominator_factors[0], denominator_factors[1])
        .ok_or(ModelError::ArithmeticExhausted)?;
    let mut quotient = Wide::ZERO;
    let mut remainder = Wide::ZERO;
    for position in (0..256).rev() {
        let (carry, shifted) = remainder.shl1();
        let shifted = Wide {
            limbs: [
                shifted.limbs[0] | u64::from(numerator.bit(position)),
                shifted.limbs[1],
                shifted.limbs[2],
                shifted.limbs[3],
            ],
        };
        let bit = carry || shifted.ge(denominator);
        remainder = if bit {
            shifted.wrapping_sub(denominator)
        } else {
            shifted
        };
        quotient = quotient.shl1().1;
        quotient.set_low_bit(bit);
    }
    if remainder != Wide::ZERO {
        quotient = quotient
            .checked_add_one()
            .ok_or(ModelError::ArithmeticExhausted)?;
    }
    quotient.low_u128()
}

/// Round `a*b/denominator` to the nearest integer, using ties-to-even.  The
/// product is deliberately evaluated through the checked fixed-width helper
/// so reweight and clock-denominator changes retain their overflow fallback.
fn checked_mul_div_round_nearest_even(
    a: u128,
    b: u128,
    denominator: u128,
) -> Result<u128, ModelError> {
    let (quotient, remainder) = checked_mul_div_rem(a, b, denominator)?;
    // Comparing `remainder` with `denominator - remainder` avoids forming
    // the potentially overflowing value `2 * remainder`.
    let round_up = match remainder.cmp(&(denominator / 2)) {
        core::cmp::Ordering::Greater => true,
        core::cmp::Ordering::Less => false,
        core::cmp::Ordering::Equal => {
            // An exact half rounds to the even quotient.  An odd denominator
            // has no exact half, so its floor-half remainder rounds down.
            denominator & 1 == 0 && quotient & 1 != 0
        }
    };
    if round_up {
        quotient
            .checked_add(1)
            .ok_or(ModelError::ArithmeticExhausted)
    } else {
        Ok(quotient)
    }
}

/// Monotonic signed-to-unsigned key bias for a future u128 augmentation.
pub const fn bias_i128(value: i128) -> u128 {
    (value as u128) ^ (1u128 << 127)
}

pub const fn unbias_i128(value: u128) -> i128 {
    (value ^ (1u128 << 127)) as i128
}

fn checked_work(ticks: u128) -> Result<i128, ModelError> {
    checked_i128(
        ticks
            .checked_mul(WORK)
            .ok_or(ModelError::ArithmeticExhausted)?,
    )
}

fn checked_virtual_length(q: u128, weight: u128) -> Result<i128, ModelError> {
    if weight == 0 {
        return Err(ModelError::InvalidWeight);
    }
    checked_i128(checked_mul_div_ceil(q, WORK, weight)?)
}

fn checked_lag_at(lag: i128, stamp: i128, weight: u128, v: i128) -> Result<i128, ModelError> {
    if weight == 0 {
        return Err(ModelError::InvalidWeight);
    }
    let delta = v
        .checked_sub(stamp)
        .ok_or(ModelError::ArithmeticExhausted)?;
    let change = checked_mul_i128_u128(delta, weight)?;
    lag.checked_add(change)
        .ok_or(ModelError::ArithmeticExhausted)
}

fn checked_deadline(start: i128, length: i128) -> Result<i128, ModelError> {
    start
        .checked_add(length)
        .ok_or(ModelError::ArithmeticExhausted)
}

fn request_quantum(
    class: RequestClass,
    weight: u128,
    total_weight: u128,
) -> Result<u128, ModelError> {
    if weight == 0 || total_weight == 0 {
        return Err(ModelError::InvalidWeight);
    }
    let target = class.target_ticks();
    if weight >= total_weight {
        // The unclamped mathematical quotient is at least `target`; avoid
        // constructing a product that is known to be discarded by the clamp.
        return Ok(target);
    }
    let q = checked_mul_div_ceil(target, weight, total_weight)?;
    Ok(core::cmp::min(core::cmp::max(q, 1), target))
}

/// Compute `ceil(-value/denominator)` without ever evaluating `-value`.
/// In particular, this handles `value == i128::MIN`, whose mathematical
/// magnitude is representable only as an unsigned 128-bit value.
fn signed_ceil_neg_div(value: i128, denominator: u128) -> Result<i128, ModelError> {
    if denominator == 0 {
        return Err(ModelError::InvalidWeight);
    }
    let magnitude = value.unsigned_abs();
    if value >= 0 {
        let quotient = magnitude / denominator;
        if quotient == 0 {
            Ok(0)
        } else {
            Ok(-(quotient as i128))
        }
    } else {
        checked_i128(checked_ceil_div(magnitude, denominator)?)
    }
}

/// Request start formula.  Positive materialized lag intentionally produces
/// an `S` before the current clock point; eligibility remains a separate
/// predicate based on the lag stamp.
pub fn request_start(v: i128, materialized_lag: i128, weight: u128) -> Result<i128, ModelError> {
    let offset = signed_ceil_neg_div(materialized_lag, weight)?;
    v.checked_add(offset).ok_or(ModelError::ArithmeticExhausted)
}

/// A current EEVDF request. `q` is the admitted target and
/// `remaining_fraction_num/den` is the exact bounded rational fraction of the
/// current request which remains. `remaining_work` and `remaining_ticks` are
/// ceil projections used by integer scheduler paths. Keeping the fraction
/// independent of `q` means A -> B -> A reconfiguration does not repeatedly
/// ceil the same request boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub class: RequestClass,
    pub q: u128,
    pub remaining_ticks: u128,
    /// Ceil projection of the exact remaining service in fixed work units.
    pub remaining_work: u128,
    /// Exact remaining-request fraction numerator.
    pub remaining_fraction_num: u128,
    /// Exact remaining-request fraction denominator.
    pub remaining_fraction_den: u128,
    pub virtual_length: i128,
    pub start: i128,
    pub deadline: i128,
}

impl Request {
    fn projection(
        q: u128,
        fraction_num: u128,
        fraction_den: u128,
    ) -> Result<(u128, u128), ModelError> {
        if q == 0 || fraction_den == 0 || fraction_num > fraction_den {
            return Err(ModelError::InvalidState);
        }
        let (whole_ticks, fractional_ticks) = checked_mul_div_rem(fraction_num, q, fraction_den)?;
        let fractional_work = if fractional_ticks == 0 {
            0
        } else {
            checked_mul_div_ceil(fractional_ticks, WORK, fraction_den)?
        };
        let whole_work = whole_ticks
            .checked_mul(WORK)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let remaining_work = whole_work
            .checked_add(fractional_work)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let remaining_ticks = checked_ceil_div(remaining_work, WORK)?;
        Ok((remaining_work, remaining_ticks))
    }

    fn exact_valid(&self) -> Result<(), ModelError> {
        let (remaining_work, remaining_ticks) = Self::projection(
            self.q,
            self.remaining_fraction_num,
            self.remaining_fraction_den,
        )?;
        if remaining_work != self.remaining_work || remaining_ticks != self.remaining_ticks {
            return Err(ModelError::InvalidState);
        }
        Ok(())
    }

    /// Calculate the remaining virtual length directly from the exact
    /// bounded fraction instead of from its ceil projection.  The projected
    /// `remaining_work` is intentionally retained for integer-facing state,
    /// but using it to re-anchor a request would make A -> B -> A updates
    /// accumulate a rounding unit on every round trip.
    fn remaining_virtual_length(&self, weight: u128) -> Result<i128, ModelError> {
        if weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        self.exact_valid()?;
        checked_i128(checked_ratio_ceil(
            [self.remaining_fraction_num, self.q, WORK],
            [self.remaining_fraction_den, weight],
        )?)
    }

    fn set_fraction(&mut self, fraction_num: u128, fraction_den: u128) -> Result<(), ModelError> {
        let gcd = gcd_u128(fraction_num, fraction_den);
        let (fraction_num, fraction_den) = match gcd {
            0 => (0, 1),
            gcd => (
                fraction_num
                    .checked_div(gcd)
                    .ok_or(ModelError::InvalidState)?,
                fraction_den
                    .checked_div(gcd)
                    .ok_or(ModelError::InvalidState)?,
            ),
        };
        let (remaining_work, remaining_ticks) =
            Self::projection(self.q, fraction_num, fraction_den)?;
        self.remaining_fraction_num = fraction_num;
        self.remaining_fraction_den = fraction_den;
        self.remaining_work = remaining_work;
        self.remaining_ticks = remaining_ticks;
        Ok(())
    }

    fn next_after_service(&self, work: u128, saturating: bool) -> Result<Self, ModelError> {
        self.exact_valid()?;
        if work % NICE_0 != 0 {
            return Err(ModelError::InvalidState);
        }
        let service_ticks = work / NICE_0;
        let service_den = self
            .q
            .checked_mul(ONE)
            .ok_or(ModelError::ArithmeticExhausted)?;

        // Compare the requested service with the exact remaining fraction
        // before constructing a common denominator.  Besides making explicit
        // exhaustion cheap, this avoids multiplying a long-running bulk
        // sample by a potentially large rational denominator merely to learn
        // that the request is already complete.
        let (remaining_service, remaining_remainder) = checked_mul_div_rem(
            self.remaining_fraction_num,
            service_den,
            self.remaining_fraction_den,
        )?;
        if service_ticks > remaining_service {
            if !saturating {
                return Err(ModelError::InvalidState);
            }
            let mut next = *self;
            next.set_fraction(0, 1)?;
            return Ok(next);
        }
        if service_ticks == remaining_service && remaining_remainder == 0 {
            let mut next = *self;
            next.set_fraction(0, 1)?;
            return Ok(next);
        }
        if service_ticks == 0 {
            return Ok(*self);
        }

        // Reduce the service fraction before taking the common denominator.
        // Without this step, repeated whole-tick samples retain factors that
        // are already present in the numerator and make the stored LCM grow
        // needlessly across reconfiguration/wakeup cycles.
        let service_gcd = gcd_u128(service_ticks, service_den);
        let service_num = service_ticks / service_gcd;
        let service_den = service_den / service_gcd;
        let denominator_gcd = gcd_u128(self.remaining_fraction_den, service_den);
        let left_den = self.remaining_fraction_den / denominator_gcd;
        let right_den = service_den / denominator_gcd;

        // After both input fractions are reduced, the numerator of
        //
        //     n * right_den - service_num * left_den
        //
        // is coprime to `left_den` and `right_den`.  Any remaining common
        // factor of the result and its common denominator must therefore be
        // a factor of `denominator_gcd`.  Find that factor modulo the small
        // gcd before constructing the denominator, so a reducible LCM never
        // has to fit in u128 merely to be cancelled afterwards.
        let mut next = *self;
        let (numerator, reduced_gcd) = match (
            self.remaining_fraction_num.checked_mul(right_den),
            service_num.checked_mul(left_den),
        ) {
            (Some(left), Some(right)) => {
                let numerator = left.checked_sub(right).ok_or(ModelError::InvalidState)?;
                let reduced_gcd = gcd_u128(numerator, denominator_gcd);
                (numerator / reduced_gcd, reduced_gcd)
            }
            _ => {
                let left = Wide::mul_u128(self.remaining_fraction_num, right_den)
                    .ok_or(ModelError::ArithmeticExhausted)?;
                let right =
                    Wide::mul_u128(service_num, left_den).ok_or(ModelError::ArithmeticExhausted)?;
                if left.cmp(right) == core::cmp::Ordering::Less {
                    return Err(ModelError::InvalidState);
                }
                let numerator = left.wrapping_sub(right);
                let reduced_gcd = if denominator_gcd == 1 {
                    1
                } else {
                    let left_mod = numerator.rem_u128(denominator_gcd)?;
                    // `right_mod` is not needed separately: the wide
                    // difference already contains both products.  Taking the
                    // remainder after subtraction avoids any u128 product.
                    gcd_u128(left_mod, denominator_gcd)
                };
                let numerator = numerator.div_exact_u128(reduced_gcd)?.low_u128()?;
                (numerator, reduced_gcd)
            }
        };
        let denominator = left_den
            .checked_mul(denominator_gcd / reduced_gcd)
            .and_then(|value| value.checked_mul(right_den))
            .ok_or(ModelError::ArithmeticExhausted)?;
        next.set_fraction(numerator, denominator)?;
        Ok(next)
    }

    pub fn new(
        class: RequestClass,
        weight: u128,
        total_weight: u128,
        v: i128,
        materialized_lag: i128,
    ) -> Result<Self, ModelError> {
        let q = request_quantum(class, weight, total_weight)?;
        let virtual_length = checked_virtual_length(q, weight)?;
        let start = request_start(v, materialized_lag, weight)?;
        let deadline = checked_deadline(start, virtual_length)?;
        let mut request = Self {
            class,
            q,
            remaining_ticks: q,
            remaining_work: 0,
            remaining_fraction_num: 1,
            remaining_fraction_den: 1,
            virtual_length,
            start,
            deadline,
        };
        request.set_fraction(1, 1)?;
        Ok(request)
    }

    pub fn admit(
        class: RequestClass,
        weight: u128,
        total_weight: u128,
        v: i128,
        materialized_lag: i128,
    ) -> Result<Self, ModelError> {
        Self::new(class, weight, total_weight, v, materialized_lag)
    }

    pub const fn remaining(&self) -> u128 {
        self.remaining_ticks
    }

    pub const fn is_exhausted(&self) -> bool {
        self.remaining_fraction_num == 0
    }

    pub fn consume(&mut self, ticks: u128) -> Result<(), ModelError> {
        if ticks > self.remaining_ticks {
            return Err(ModelError::InvalidState);
        }
        self.consume_work_up_to(checked_work(ticks)? as u128)
    }

    /// Consume an exact fixed-width amount of service, saturating only at the
    /// request boundary.  This is used by fractional runtime settlement and
    /// keeps the integer `remaining_ticks` projection synchronized.
    pub fn consume_work(&mut self, work: u128) -> Result<(), ModelError> {
        *self = self.next_after_service(work, false)?;
        Ok(())
    }

    /// Consume service up to the exact request boundary.  A timer sample can
    /// legitimately overshoot a request; the entity records that overservice
    /// in lag while the request itself saturates at zero.
    pub fn consume_work_up_to(&mut self, work: u128) -> Result<(), ModelError> {
        *self = self.next_after_service(work, true)?;
        Ok(())
    }

    /// Rebind the request quantum without changing the exact remaining
    /// fraction.  This is the key operation used by class/weight updates.
    fn set_quantum(&mut self, q: u128) -> Result<(), ModelError> {
        if q == 0 {
            return Err(ModelError::InvalidState);
        }
        let (remaining_work, remaining_ticks) =
            Self::projection(q, self.remaining_fraction_num, self.remaining_fraction_den)?;
        self.q = q;
        self.remaining_work = remaining_work;
        self.remaining_ticks = remaining_ticks;
        Ok(())
    }

    pub fn renew(
        &mut self,
        weight: u128,
        total_weight: u128,
        v: i128,
        materialized_lag: i128,
    ) -> Result<(), ModelError> {
        let next = Self::new(self.class, weight, total_weight, v, materialized_lag)?;
        *self = next;
        Ok(())
    }

    pub fn yielded(
        &self,
        weight: u128,
        total_weight: u128,
        v: i128,
        materialized_lag: i128,
    ) -> Result<Self, ModelError> {
        let mut next = Self::new(self.class, weight, total_weight, v, materialized_lag)?;
        next.start = core::cmp::max(next.start, self.deadline);
        next.deadline = checked_deadline(next.start, next.virtual_length)?;
        Ok(next)
    }

    pub const fn preempted(&self) -> Self {
        *self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clock {
    pub v: i128,
    pub remainder: u128,
    pub total_weight: u128,
    pub accounted_ticks: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockAdvance {
    pub v: i128,
    pub remainder: u128,
    pub accounted_ticks: u128,
    pub delta_v: i128,
}

impl Clock {
    pub const fn new(total_weight: u128) -> Self {
        Self {
            v: 0,
            remainder: 0,
            total_weight,
            accounted_ticks: 0,
        }
    }

    pub const fn at(v: i128, total_weight: u128) -> Self {
        Self {
            v,
            remainder: 0,
            total_weight,
            accounted_ticks: 0,
        }
    }

    pub const fn with_parts(
        v: i128,
        remainder: u128,
        total_weight: u128,
        accounted_ticks: u128,
    ) -> Self {
        Self {
            v,
            // There is no denominator in an empty fair queue.  Discarding a
            // stale residue here prevents it from being presented as
            // conserved work when the first entity is admitted later.
            remainder: if total_weight == 0 { 0 } else { remainder },
            total_weight,
            accounted_ticks,
        }
    }

    fn preview_work_with_ticks(
        &self,
        work: u128,
        accounted_tick_delta: u128,
    ) -> Result<ClockAdvance, ModelError> {
        if work == 0 {
            let accounted_ticks = self
                .accounted_ticks
                .checked_add(accounted_tick_delta)
                .ok_or(ModelError::ArithmeticExhausted)?;
            return Ok(ClockAdvance {
                v: self.v,
                remainder: self.remainder,
                accounted_ticks,
                delta_v: 0,
            });
        }
        if self.total_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.remainder >= self.total_weight {
            return Err(ModelError::InvalidState);
        }
        let (mut delta_v_u128, product_remainder) =
            checked_mul_div_rem(work, 1, self.total_weight)?;
        let (remainder_low, remainder_high) = product_remainder.overflowing_add(self.remainder);
        let (remainder_quotient, remainder) =
            checked_div_129(remainder_high, remainder_low, self.total_weight)?;
        delta_v_u128 = delta_v_u128
            .checked_add(remainder_quotient)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let delta_v = checked_i128(delta_v_u128)?;
        let v = self
            .v
            .checked_add(delta_v)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let accounted_ticks = self
            .accounted_ticks
            .checked_add(accounted_tick_delta)
            .ok_or(ModelError::ArithmeticExhausted)?;
        Ok(ClockAdvance {
            v,
            remainder,
            accounted_ticks,
            delta_v,
        })
    }

    /// Preview advancing virtual time by an exact fixed-point work amount.
    /// Unlike `preview_advance`, this does not increment the integer tick
    /// counter; fractional runtime settlement is observable through work
    /// balance and must not manufacture a scheduler tick.
    pub fn preview_work(&self, work: u128) -> Result<ClockAdvance, ModelError> {
        self.preview_work_with_ticks(work, 0)
    }

    pub fn preview_advance(&self, ticks: u128) -> Result<ClockAdvance, ModelError> {
        if ticks == 0 {
            return self.preview_work_with_ticks(0, 0);
        }
        if self.total_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.remainder >= self.total_weight {
            return Err(ModelError::InvalidState);
        }
        // Keep the tick-to-work multiplication inside the checked fixed-width
        // quotient helper.  A long VM pause may make `ticks * WORK` exceed
        // u128 even though the resulting virtual-time delta is representable.
        let (mut delta_v_u128, product_remainder) =
            checked_mul_div_rem(ticks, WORK, self.total_weight)?;
        let (remainder_low, remainder_high) = product_remainder.overflowing_add(self.remainder);
        let (remainder_quotient, remainder) =
            checked_div_129(remainder_high, remainder_low, self.total_weight)?;
        delta_v_u128 = delta_v_u128
            .checked_add(remainder_quotient)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let delta_v = checked_i128(delta_v_u128)?;
        let v = self
            .v
            .checked_add(delta_v)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let accounted_ticks = self
            .accounted_ticks
            .checked_add(ticks)
            .ok_or(ModelError::ArithmeticExhausted)?;
        Ok(ClockAdvance {
            v,
            remainder,
            accounted_ticks,
            delta_v,
        })
    }

    pub fn advance_ticks(&mut self, ticks: u128) -> Result<i128, ModelError> {
        let next = self.preview_advance(ticks)?;
        *self = Self {
            v: next.v,
            remainder: next.remainder,
            total_weight: self.total_weight,
            accounted_ticks: next.accounted_ticks,
        };
        Ok(next.delta_v)
    }

    /// Advance virtual time by exact fixed-point work without incrementing
    /// `accounted_ticks`.
    pub fn advance_work(&mut self, work: u128) -> Result<i128, ModelError> {
        let next = self.preview_work(work)?;
        *self = Self {
            v: next.v,
            remainder: next.remainder,
            total_weight: self.total_weight,
            accounted_ticks: self.accounted_ticks,
        };
        Ok(next.delta_v)
    }

    pub fn advance(&mut self, ticks: u128) -> Result<i128, ModelError> {
        self.advance_ticks(ticks)
    }

    pub fn jump_to(&mut self, v: i128) -> Result<(), ModelError> {
        if v < self.v {
            return Err(ModelError::InvalidState);
        }
        self.v = v;
        Ok(())
    }

    pub fn jump_to_eligibility(&mut self, entity: &Entity) -> Result<(), ModelError> {
        self.jump_to(entity.eligible_at()?)
    }

    pub fn set_total_weight(&mut self, total_weight: u128) -> Result<(), ModelError> {
        let old_weight = self.total_weight;
        if old_weight == total_weight {
            if total_weight == 0 {
                self.remainder = 0;
            }
            return Ok(());
        }

        // An empty clock has no meaningful fractional denominator.  Likewise
        // dropping the final entity must discard the now-unattached residue.
        if old_weight == 0 || total_weight == 0 {
            self.total_weight = total_weight;
            self.remainder = 0;
            return Ok(());
        }

        // A valid clock residue is always strictly below its denominator.
        // Check this before doing any work so malformed state is rejected
        // without partially changing the clock.
        if self.remainder >= old_weight {
            return Err(ModelError::InvalidState);
        }
        let rounded = checked_mul_div_round_nearest_even(self.remainder, total_weight, old_weight)?;
        let (v, remainder) = match rounded.cmp(&total_weight) {
            core::cmp::Ordering::Equal => (
                self.v
                    .checked_add(1)
                    .ok_or(ModelError::ArithmeticExhausted)?,
                0,
            ),
            core::cmp::Ordering::Less => (self.v, rounded),
            core::cmp::Ordering::Greater => {
                // This is unreachable for a valid residue, but keeping the
                // representation check explicit makes the operation atomic
                // even if a future caller relaxes the invariant above.
                return Err(ModelError::InvalidState);
            }
        };
        self.v = v;
        self.remainder = remainder;
        self.total_weight = total_weight;
        Ok(())
    }

    pub fn checked_add_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        let total_weight = self
            .total_weight
            .checked_add(weight)
            .ok_or(ModelError::ArithmeticExhausted)?;
        self.set_total_weight(total_weight)
    }

    pub fn checked_sub_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        let total_weight = self
            .total_weight
            .checked_sub(weight)
            .ok_or(ModelError::InvalidWeight)?;
        self.set_total_weight(total_weight)
    }

    pub fn add_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_add_weight(weight)
    }

    pub fn sub_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_sub_weight(weight)
    }

    pub fn checked_add(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_add_weight(weight)
    }

    pub fn checked_sub(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_sub_weight(weight)
    }

    pub fn checked_add_total_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_add_weight(weight)
    }

    pub fn checked_sub_total_weight(&mut self, weight: u128) -> Result<(), ModelError> {
        self.checked_sub_weight(weight)
    }

    pub fn work_balance(&self) -> Result<i128, ModelError> {
        if self.total_weight == 0 {
            return if self.remainder == 0 {
                Ok(0)
            } else {
                Err(ModelError::InvalidState)
            };
        }
        checked_mul_i128_u128(self.v, self.total_weight)?
            .checked_add(checked_i128(self.remainder)?)
            .ok_or(ModelError::ArithmeticExhausted)
    }
}

pub const fn credit_cap(class: RequestClass) -> i128 {
    (class.target_ticks() * WORK) as i128
}

fn bounded_sleeper_decay_inner(
    class: RequestClass,
    lag: i128,
    elapsed: i128,
) -> Result<i128, ModelError> {
    let cap = credit_cap(class);
    let lag = if lag > cap { cap } else { lag };
    if elapsed < 0 {
        return Err(ModelError::InvalidState);
    }
    if elapsed <= GRACE {
        return Ok(lag);
    }
    let beyond = core::cmp::min(
        elapsed
            .checked_sub(GRACE)
            .ok_or(ModelError::ArithmeticExhausted)?,
        DECAY_WINDOW,
    );
    if beyond >= DECAY_WINDOW {
        return Ok(0);
    }
    let remaining = (DECAY_WINDOW)
        .checked_sub(beyond)
        .ok_or(ModelError::ArithmeticExhausted)?;
    if lag == 0 {
        return Ok(0);
    }
    let magnitude = lag.unsigned_abs();
    let scaled = checked_mul_div_ceil(magnitude, remaining as u128, DECAY_WINDOW as u128)?;
    if lag < 0 {
        if scaled == (1u128 << 127) {
            Ok(i128::MIN)
        } else {
            Ok(-(scaled as i128))
        }
    } else {
        checked_i128(core::cmp::min(scaled, cap as u128))
    }
}

#[cfg(test)]
pub fn bounded_sleeper_decay(
    class: RequestClass,
    lag: i128,
    elapsed: i128,
) -> Result<i128, ModelError> {
    bounded_sleeper_decay_inner(class, lag, elapsed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationSnapshot {
    pub class: RequestClass,
    pub weight: u128,
    pub request: Request,
    pub lag: i128,
    pub source_v: i128,
    pub start_offset: i128,
    pub deadline_offset: i128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entity {
    pub class: RequestClass,
    pub weight: u128,
    pub lag: i128,
    pub lag_stamp: i128,
    pub request: Request,
    sleeper_v: Option<i128>,
}

impl Entity {
    pub fn new(
        class: RequestClass,
        weight: u128,
        total_weight: u128,
        v: i128,
    ) -> Result<Self, ModelError> {
        Self::with_lag(class, weight, total_weight, v, 0)
    }

    pub fn with_lag(
        class: RequestClass,
        weight: u128,
        total_weight: u128,
        v: i128,
        lag: i128,
    ) -> Result<Self, ModelError> {
        let request = Request::new(class, weight, total_weight, v, lag)?;
        Ok(Self {
            class,
            weight,
            lag,
            lag_stamp: v,
            request,
            sleeper_v: None,
        })
    }

    pub fn from_weight(
        weight: u128,
        total_weight: u128,
        class: RequestClass,
        v: i128,
    ) -> Result<Self, ModelError> {
        Self::new(class, weight, total_weight, v)
    }

    pub fn is_sleeping(&self) -> bool {
        self.sleeper_v.is_some()
    }

    /// Return the virtual-time point at which this entity entered sleep.
    ///
    /// The scheduler keeps the entity itself while a fair task sleeps.  A
    /// parameter update must therefore retain this anchor rather than
    /// treating the update as a wakeup/re-admission.
    pub const fn sleep_anchor(&self) -> Option<i128> {
        self.sleeper_v
    }

    pub fn lag_at(&self, v: i128) -> Result<i128, ModelError> {
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Ok(self.lag);
        }
        checked_lag_at(self.lag, self.lag_stamp, self.weight, v)
    }

    pub fn materialize(&mut self, v: i128) -> Result<i128, ModelError> {
        let next = self.lag_at(v)?;
        if self.sleeper_v.is_none() {
            // Preserve the exact checked lag at the materialization point.
            // A negative lag may cross zero between representable points, and
            // the positive overshoot is real credit for the next request.
            self.lag = next;
            self.lag_stamp = v;
        }
        Ok(self.lag)
    }

    /// Freeze an entity at `v` before removing it from fair accounting.
    ///
    /// A dormant entity is not represented in `Clock.total_weight`, so the
    /// fair debt must be materialized exactly once at the class transition.
    /// Callers that retain the entity while it is in an RT class must use the
    /// frozen reconfiguration path below when it becomes fair again.
    pub fn freeze_at(&mut self, v: i128) -> Result<i128, ModelError> {
        self.materialize(v)
    }

    pub fn eligible_at(&self) -> Result<i128, ModelError> {
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.lag >= 0 {
            return Ok(self.lag_stamp);
        }
        let offset = checked_i128(checked_ceil_div(self.lag.unsigned_abs(), self.weight)?)?;
        self.lag_stamp
            .checked_add(offset)
            .ok_or(ModelError::ArithmeticExhausted)
    }

    pub fn is_eligible(&self, v: i128) -> Result<bool, ModelError> {
        Ok(self.lag_at(v)? >= 0)
    }

    /// Account one transaction containing whole and fractional service.
    /// Every checked operation is staged through local copies before either
    /// the queue clock or entity is changed.  This is the atomicity boundary
    /// used by both timer accounting and lifecycle settlement.
    pub fn service(
        &mut self,
        clock: &mut Clock,
        ticks: u128,
        fraction_q: u128,
    ) -> Result<i128, ModelError> {
        if fraction_q >= ONE {
            return Err(ModelError::InvalidState);
        }
        if ticks == 0 && fraction_q == 0 {
            return Ok(0);
        }
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let whole_work = checked_work(ticks)? as u128;
        let fractional_work = fraction_q
            .checked_mul(NICE_0)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let service_work = whole_work
            .checked_add(fractional_work)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let next_clock = clock.preview_work_with_ticks(service_work, ticks)?;
        let next_lag = checked_lag_at(self.lag, self.lag_stamp, self.weight, next_clock.v)?
            .checked_sub(checked_i128(service_work)?)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let mut next_request = self.request;
        next_request.consume_work_up_to(service_work)?;

        // Commit only after clock, lag, and request arithmetic all succeeds.
        clock.v = next_clock.v;
        clock.remainder = next_clock.remainder;
        clock.accounted_ticks = next_clock.accounted_ticks;
        self.lag = next_lag;
        self.lag_stamp = next_clock.v;
        self.request = next_request;
        Ok(next_clock.delta_v)
    }

    pub fn tick_service(&mut self, clock: &mut Clock, ticks: u128) -> Result<i128, ModelError> {
        self.service(clock, ticks, 0)
    }

    /// Account a contiguous run of service in one model transaction.
    ///
    /// Unlike [`Self::tick_service`], this method deliberately accepts more
    /// service than the active request has left.  A missed timer boundary is
    /// still real service: the clock and lag advance for every tick and the
    /// request is consumed only up to zero.  The resulting lag therefore
    /// retains the task's overservice debt for the scheduler's next renewal.
    /// All arithmetic is fixed-width and independent of the numerical size of
    /// `ticks`; callers can safely use this at a wall-clock ownership boundary
    /// after a long VM pause.
    pub fn bulk_service(&mut self, clock: &mut Clock, ticks: u128) -> Result<i128, ModelError> {
        self.service(clock, ticks, 0)
    }

    /// Account a bounded sub-tick service amount in Q32 tick units. The
    /// operation advances the fair clock and lag by exact fixed work while
    /// leaving the integer scheduler tick counter unchanged.
    pub fn fractional_service(
        &mut self,
        clock: &mut Clock,
        fraction_q: u64,
    ) -> Result<i128, ModelError> {
        self.service(clock, 0, u128::from(fraction_q))
    }

    /// Convert a wall-clock sub-period into Q32 service and materialize it.
    /// The returned division remainder is bounded by `period_ns`; callers may
    /// retain it in a fixed-width boundary token when they need conservation
    /// across multiple samples.
    pub fn fractional_service_ns(
        &mut self,
        clock: &mut Clock,
        elapsed_ns: u64,
        period_ns: u64,
    ) -> Result<u64, ModelError> {
        if period_ns == 0 || elapsed_ns >= period_ns {
            return Err(ModelError::InvalidState);
        }
        let (fraction_q, remainder) =
            checked_mul_div_rem(u128::from(elapsed_ns), ONE, u128::from(period_ns))?;
        self.fractional_service(clock, fraction_q as u64)?;
        Ok(remainder as u64)
    }

    pub fn tick(&mut self, clock: &mut Clock) -> Result<i128, ModelError> {
        self.tick_service(clock, 1)
    }

    pub fn renew(&mut self, total_weight: u128, v: i128) -> Result<(), ModelError> {
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let next = Request::new(self.class, self.weight, total_weight, v, lag)?;
        self.lag = lag;
        self.lag_stamp = v;
        self.request = next;
        Ok(())
    }

    pub fn yield_request(&mut self, total_weight: u128, v: i128) -> Result<(), ModelError> {
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let next = self.request.yielded(self.weight, total_weight, v, lag)?;
        self.lag = lag;
        self.lag_stamp = v;
        self.request = next;
        Ok(())
    }

    pub fn preempt_request(&mut self) -> Result<(), ModelError> {
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        Ok(())
    }

    pub fn begin_sleep(&mut self, v: i128) -> Result<(), ModelError> {
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let lag = core::cmp::min(lag, credit_cap(self.class));
        self.lag = lag;
        self.lag_stamp = v;
        self.sleeper_v = Some(v);
        Ok(())
    }

    /// Mark a frozen dormant entity as sleeping without materializing the
    /// virtual time elapsed while it was represented by RT.
    pub fn begin_sleep_frozen(&mut self, v: i128) -> Result<(), ModelError> {
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        self.lag_stamp = v;
        self.sleeper_v = Some(v);
        Ok(())
    }

    pub fn sleep(&mut self, v: i128) -> Result<(), ModelError> {
        self.begin_sleep(v)
    }

    pub fn wake(&mut self, v: i128) -> Result<(), ModelError> {
        let slept_at = self.sleeper_v.ok_or(ModelError::InvalidState)?;
        let elapsed = v
            .checked_sub(slept_at)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let lag = bounded_sleeper_decay_inner(self.class, self.lag, elapsed)?;
        self.lag = lag;
        self.lag_stamp = v;
        self.sleeper_v = None;
        Ok(())
    }

    /// End a dormant fair sleeping lifetime without sleeper credit.
    ///
    /// This is used when a fair sleeper was temporarily represented by an RT
    /// sleep marker.  No grace/decay credit is applied, and an already-active
    /// frozen dormant entity is also re-anchored without materializing the
    /// virtual time elapsed while the task was RT.
    pub fn end_sleep_frozen(&mut self, v: i128) -> Result<(), ModelError> {
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if let Some(slept_at) = self.sleeper_v {
            if v < slept_at {
                return Err(ModelError::InvalidState);
            }
        }
        self.sleeper_v = None;
        self.lag_stamp = v;
        Ok(())
    }

    /// Wake while preserving request progress already converted for the
    /// eventual runnable aggregate.
    ///
    /// Sleeper decay still applies to lag, but `q` and `remaining_ticks` are
    /// not rescaled a second time.  The request is merely re-anchored at the
    /// wake point using its existing remaining virtual length.
    pub fn wake_preserving_progress(&mut self, v: i128) -> Result<(), ModelError> {
        let slept_at = self.sleeper_v.ok_or(ModelError::InvalidState)?;
        let elapsed = v
            .checked_sub(slept_at)
            .ok_or(ModelError::ArithmeticExhausted)?;
        if self.weight == 0 || self.request.q == 0 {
            return Err(ModelError::InvalidState);
        }
        self.request.exact_valid()?;
        let lag = bounded_sleeper_decay_inner(self.class, self.lag, elapsed)?;
        let start = request_start(v, lag, self.weight)?;
        let remaining_r = self.request.remaining_virtual_length(self.weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        let mut request = self.request;
        request.start = start;
        request.deadline = deadline;
        *self = Self {
            class: self.class,
            weight: self.weight,
            lag,
            lag_stamp: v,
            request,
            sleeper_v: None,
        };
        Ok(())
    }

    /// Re-anchor an already-woken request at a clock rebase point without
    /// changing its converted progress.  Clock denominator changes may move
    /// `V` by one; this keeps lag/request coordinates consistent with that
    /// final clock while retaining `q` and `remaining_ticks` exactly.
    pub fn activate_preserving_progress(&mut self, v: i128) -> Result<(), ModelError> {
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.request.q == 0 {
            return Err(ModelError::InvalidState);
        }
        self.request.exact_valid()?;
        let lag = self.lag_at(v)?;
        let start = request_start(v, lag, self.weight)?;
        let remaining_r = self.request.remaining_virtual_length(self.weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        let mut request = self.request;
        request.start = start;
        request.deadline = deadline;
        self.lag = lag;
        self.lag_stamp = v;
        self.request = request;
        Ok(())
    }

    pub fn wake_at(&mut self, v: i128) -> Result<(), ModelError> {
        self.wake(v)
    }

    fn reconfigured_request(
        &self,
        new_class: RequestClass,
        new_q: u128,
        new_weight: u128,
        anchor: i128,
        lag: i128,
    ) -> Result<Request, ModelError> {
        self.request.exact_valid()?;
        let mut request = self.request;
        request.class = new_class;
        request.set_quantum(new_q)?;
        let full_r = checked_virtual_length(new_q, new_weight)?;
        let remaining_r = request.remaining_virtual_length(new_weight)?;
        let start = request_start(anchor, lag, new_weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        request.virtual_length = full_r;
        request.start = start;
        request.deadline = deadline;
        Ok(request)
    }

    /// Changes class and weight while preserving materialized lag and the
    /// completed fraction of the active request.  `new_total_weight` is the
    /// final fair aggregate after replacing this entity's old weight.
    ///
    /// All checked calculations are completed before `self` is assigned, so
    /// arithmetic failures leave the entity byte-for-byte unchanged.
    pub fn reconfigure(
        &mut self,
        new_class: RequestClass,
        new_weight: u128,
        new_total_weight: u128,
        v: i128,
    ) -> Result<(), ModelError> {
        if new_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let request = self.reconfigured_request(new_class, new_q, new_weight, v, lag)?;
        let next = Self {
            class: new_class,
            weight: new_weight,
            lag,
            lag_stamp: v,
            request,
            sleeper_v: self.sleeper_v,
        };
        *self = next;
        Ok(())
    }

    /// Reconfigure a dormant fair entity without materializing elapsed clock
    /// time.  The entity was frozen when it left fair accounting; RT runtime
    /// therefore cannot create fair lag or sleeper credit.
    pub fn reconfigure_frozen(
        &mut self,
        new_class: RequestClass,
        new_weight: u128,
        new_total_weight: u128,
        v: i128,
    ) -> Result<(), ModelError> {
        if new_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        // `self.lag` is deliberately used directly: `lag_at(v)` would charge
        // all virtual time elapsed while the task was in the RT class.
        let lag = self.lag;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let request = self.reconfigured_request(new_class, new_q, new_weight, v, lag)?;
        *self = Self {
            class: new_class,
            weight: new_weight,
            lag,
            lag_stamp: v,
            request,
            sleeper_v: None,
        };
        Ok(())
    }

    /// Reconfigure a sleeping entity while retaining its sleep anchor and
    /// active-request progress.
    ///
    /// Sleeping entities do not accrue lag while absent from the run queue,
    /// so all calculations use the saved sleep anchor.  The operation is
    /// staged through locals and is byte-for-byte atomic on failure.
    pub fn reconfigure_sleeping(
        &mut self,
        new_class: RequestClass,
        new_weight: u128,
        new_total_weight: u128,
    ) -> Result<(), ModelError> {
        let sleeper_v = self.sleeper_v.ok_or(ModelError::InvalidState)?;
        if new_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        let lag = self.lag;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let request = self.reconfigured_request(new_class, new_q, new_weight, sleeper_v, lag)?;
        *self = Self {
            class: new_class,
            weight: new_weight,
            lag,
            lag_stamp: sleeper_v,
            request,
            sleeper_v: Some(sleeper_v),
        };
        Ok(())
    }

    /// Reweight while retaining the current request class.
    pub fn reweight(
        &mut self,
        new_weight: u128,
        new_total_weight: u128,
        v: i128,
    ) -> Result<(), ModelError> {
        self.reconfigure(self.class, new_weight, new_total_weight, v)
    }

    pub fn set_weight(
        &mut self,
        new_weight: u128,
        new_total_weight: u128,
        v: i128,
    ) -> Result<(), ModelError> {
        self.reweight(new_weight, new_total_weight, v)
    }

    pub fn migration_snapshot(&self, source_v: i128) -> Result<MigrationSnapshot, ModelError> {
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(source_v)?;
        Ok(MigrationSnapshot {
            class: self.class,
            weight: self.weight,
            request: self.request,
            lag,
            source_v,
            start_offset: self
                .request
                .start
                .checked_sub(source_v)
                .ok_or(ModelError::ArithmeticExhausted)?,
            deadline_offset: self
                .request
                .deadline
                .checked_sub(source_v)
                .ok_or(ModelError::ArithmeticExhausted)?,
        })
    }

    pub fn snapshot(&self, source_v: i128) -> Result<MigrationSnapshot, ModelError> {
        self.migration_snapshot(source_v)
    }

    pub fn from_migration(
        snapshot: MigrationSnapshot,
        destination_v: i128,
    ) -> Result<Self, ModelError> {
        if snapshot.weight == 0 || snapshot.request.q == 0 {
            return Err(ModelError::InvalidState);
        }
        if snapshot.request.class != snapshot.class {
            return Err(ModelError::InvalidState);
        }
        if snapshot.request.q > snapshot.class.target_ticks()
            || snapshot.request.remaining_ticks > snapshot.request.q
            || snapshot.request.remaining_fraction_den == 0
            || snapshot.request.remaining_fraction_num > snapshot.request.remaining_fraction_den
        {
            return Err(ModelError::InvalidState);
        }
        snapshot.request.exact_valid()?;
        let expected_length = checked_virtual_length(snapshot.request.q, snapshot.weight)?;
        if snapshot.request.virtual_length != expected_length {
            return Err(ModelError::InvalidState);
        }
        if snapshot.start_offset
            != snapshot
                .request
                .start
                .checked_sub(snapshot.source_v)
                .ok_or(ModelError::ArithmeticExhausted)?
            || snapshot.deadline_offset
                != snapshot
                    .request
                    .deadline
                    .checked_sub(snapshot.source_v)
                    .ok_or(ModelError::ArithmeticExhausted)?
        {
            return Err(ModelError::InvalidState);
        }
        let start = destination_v
            .checked_add(snapshot.start_offset)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let deadline = destination_v
            .checked_add(snapshot.deadline_offset)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let mut request = snapshot.request;
        request.start = start;
        request.deadline = deadline;
        Ok(Self {
            class: snapshot.class,
            weight: snapshot.weight,
            lag: snapshot.lag,
            lag_stamp: destination_v,
            request,
            sleeper_v: None,
        })
    }

    pub fn reconstruct(
        snapshot: MigrationSnapshot,
        destination_v: i128,
    ) -> Result<Self, ModelError> {
        Self::from_migration(snapshot, destination_v)
    }
}

#[cfg(test)]
pub fn min_eligible_at(entities: &[Entity]) -> Result<Option<i128>, ModelError> {
    let mut minimum = None;
    for entity in entities {
        let frontier = entity.eligible_at()?;
        minimum = Some(match minimum {
            Some(current) if current <= frontier => current,
            _ => frontier,
        });
    }
    Ok(minimum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_constants_drive_request_and_sleeper_model() {
        assert_eq!(
            RequestClass::Normal.target_ticks(),
            EEVDF_PROFILE.normal_target_ticks
        );
        assert_eq!(
            RequestClass::Batch.target_ticks(),
            EEVDF_PROFILE.batch_target_ticks
        );
        assert_eq!(
            RequestClass::Idle.target_ticks(),
            EEVDF_PROFILE.idle_target_ticks
        );
        assert_eq!(GRACE, (EEVDF_PROFILE.sleeper_grace_ticks * ONE) as i128);
        assert_eq!(
            DECAY_WINDOW,
            (EEVDF_PROFILE.sleeper_decay_ticks * ONE) as i128
        );
        assert!(EEVDF_PROFILE.has_power_of_two_parameters());
    }

    #[test]
    fn bias_is_order_preserving_at_boundaries() {
        let values = [i128::MIN, -1, 0, 1, i128::MAX];
        for pair in values.windows(2) {
            assert!(bias_i128(pair[0]) < bias_i128(pair[1]));
        }
        for value in values {
            assert_eq!(unbias_i128(bias_i128(value)), value);
        }
    }

    #[test]
    fn fixed_width_mul_div_handles_u128_boundaries_without_value_loops() {
        let ordinary_product = 3u128.checked_mul(WORK).unwrap();
        assert_eq!(
            checked_mul_div_rem(3, WORK, 10),
            Ok((ordinary_product / 10, ordinary_product % 10))
        );
        assert_eq!(checked_mul_div_ceil(u128::MAX, 1, u128::MAX), Ok(1));
        assert_eq!(
            checked_mul_div_ceil(u128::MAX, u128::MAX, u128::MAX),
            Ok(u128::MAX)
        );
        assert_eq!(
            checked_mul_div_ceil(u128::MAX, u128::MAX, u128::MAX - 1),
            Err(ModelError::ArithmeticExhausted)
        );
        assert_eq!(
            checked_mul_div_ceil(u128::MAX, u128::MAX, 1),
            Err(ModelError::ArithmeticExhausted)
        );
        let mut clock = Clock::new(u128::MAX);
        assert_eq!(clock.advance_ticks(u128::MAX), Ok(WORK as i128));
        assert_eq!(clock.remainder, 0);
        let entity = Entity::new(RequestClass::Normal, u128::MAX, 1, 0).unwrap();
        assert_eq!(entity.request.q, TARGET_TICKS_NORMAL);
    }

    #[test]
    fn fixed_width_mul_div_overflow_oracles_keep_remainders() {
        // Both products overflow u128, while each quotient remains
        // representable.  The expected values use algebraic decomposition so
        // this test does not evaluate the overflowing product itself.
        let max = u128::MAX;
        let max_over_7 = max / 7;
        assert_eq!(max % 7, 3);
        assert_eq!(checked_mul_div_rem(max, 2, 7), Ok((max_over_7 * 2, 6)));
        assert_eq!(checked_mul_div_ceil(max, 2, 7), Ok(max_over_7 * 2 + 1));

        let max_over_10 = max / 10;
        assert_eq!(max % 10, 5);
        assert_eq!(
            checked_mul_div_rem(max, 3, 10),
            Ok((max_over_10 * 3 + 1, 5))
        );
        assert_eq!(checked_mul_div_ceil(max, 3, 10), Ok(max_over_10 * 3 + 2));
    }

    #[test]
    fn fixed_width_mul_div_matches_small_reference_trace() {
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..256 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let a = (seed as u128) & ((1u128 << 63) - 1);
            seed = seed.rotate_left(17);
            let b = (seed as u128) & ((1u128 << 63) - 1);
            seed = seed.rotate_left(29);
            let denominator = ((seed as u128) & ((1u128 << 31) - 1)) + 1;
            let product = a * b;
            let expected = product / denominator + u128::from(product % denominator != 0);
            assert_eq!(checked_mul_div_ceil(a, b, denominator), Ok(expected));
        }
    }

    #[test]
    fn admission_uses_unequal_total_weight_and_every_ceil() {
        let e = Entity::new(RequestClass::Normal, 3, 10, 0).unwrap();
        let expected_q = request_quantum(RequestClass::Normal, 3, 10).unwrap();
        assert_eq!(e.request.q, expected_q);
        assert_eq!(
            e.request.virtual_length,
            checked_virtual_length(expected_q, 3).unwrap()
        );
        assert_eq!(e.request.start, 0);
        assert_eq!(
            e.request.deadline,
            checked_virtual_length(expected_q, 3).unwrap()
        );

        let small = Entity::new(RequestClass::Normal, 1, 100, 0).unwrap();
        assert_eq!(small.request.q, 1); // clamp ceil(8/100) to one
        assert_eq!(small.request.virtual_length, WORK as i128);
    }

    #[test]
    fn request_start_for_positive_and_negative_lag_is_signed_ceil() {
        assert_eq!(request_start(100, 5, 2).unwrap(), 98);
        assert_eq!(request_start(100, -5, 2).unwrap(), 103);
        assert_eq!(request_start(100, 0, 2).unwrap(), 100);
        assert_eq!(request_start(0, i128::MIN, 2).unwrap(), 1i128 << 126);
        assert_eq!(
            request_start(0, i128::MIN, 1),
            Err(ModelError::ArithmeticExhausted)
        );
    }

    #[test]
    fn eligibility_matches_materialized_lag_reference() {
        let mut entity = Entity::with_lag(RequestClass::Normal, NICE_0, NICE_0, 0, -3).unwrap();
        let frontier = entity.eligible_at().unwrap();
        for v in -2..6 {
            let expected = entity.lag_at(v).unwrap() >= 0;
            assert_eq!(entity.is_eligible(v).unwrap(), expected);
            assert_eq!(frontier <= v, expected);
        }
        entity.materialize(frontier).unwrap();
        assert_eq!(entity.lag, 1021);
    }

    #[test]
    fn materialize_preserves_positive_overshoot_credit_after_crossing_zero() {
        let mut entity = Entity::with_lag(RequestClass::Normal, 2, 2, 0, -3).unwrap();
        assert_eq!(entity.materialize(2), Ok(1));
        assert_eq!(entity.lag, 1);
        assert_eq!(entity.lag_stamp, 2);
    }

    #[test]
    fn clock_remainder_conserves_work() {
        let mut clock = Clock::new(3 * NICE_0);
        let initial = clock.work_balance().unwrap();
        for _ in 0..37 {
            clock.advance_ticks(1).unwrap();
        }
        assert_eq!(clock.work_balance().unwrap() - initial, (37 * WORK) as i128);
        assert_eq!(clock.accounted_ticks, 37);
    }

    #[test]
    fn fractional_service_complements_to_one_exact_tick() {
        let mut fractional = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut fractional_clock = Clock::new(NICE_0);
        fractional
            .fractional_service(&mut fractional_clock, (ONE / 2) as u64)
            .unwrap();
        fractional
            .fractional_service(&mut fractional_clock, (ONE - ONE / 2) as u64)
            .unwrap();

        let mut whole = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut whole_clock = Clock::new(NICE_0);
        whole.tick(&mut whole_clock).unwrap();

        assert_eq!(fractional.lag, whole.lag);
        assert_eq!(fractional.lag_stamp, whole.lag_stamp);
        assert_eq!(fractional.request, whole.request);
        assert_eq!(fractional_clock.v, whole_clock.v);
        assert_eq!(fractional_clock.remainder, whole_clock.remainder);
        assert_eq!(fractional_clock.accounted_ticks, 0);
        assert_eq!(whole_clock.accounted_ticks, 1);
    }

    #[test]
    fn rational_service_reduces_wide_lcm_before_publishing_fraction() {
        let mut request = Request::new(RequestClass::Normal, 1, 8, 0, 0).unwrap();
        // Pin the request quantum to one tick so the cancellation oracle is
        // independent of the selected EEVDF target profile.
        request.set_quantum(1).unwrap();
        // The unreduced common denominator is k * ONE, which is wider than
        // u128.  The exact result nevertheless reduces to 1/k, so rejecting
        // the intermediate product would be a false arithmetic exhaustion.
        let k = (u128::MAX - (ONE - 1)) / ONE;
        let denominator = k * ONE;
        request.set_fraction(k + ONE, denominator).unwrap();
        request.consume_work(NICE_0).unwrap();
        assert_eq!(
            (
                request.remaining_fraction_num,
                request.remaining_fraction_den
            ),
            (1, k)
        );
    }

    #[test]
    fn wide_ratio_ceil_keeps_valid_reconfigure_virtual_length() {
        // Both denominator factors are individually representable but their
        // product is not.  The mathematical result is one fixed-point unit,
        // and the bounded wide division must preserve that result.
        assert_eq!(
            checked_ratio_ceil([1, 1, WORK], [u128::MAX, u128::MAX]),
            Ok(1)
        );
    }

    #[test]
    fn wide_products_match_u128_boundary_limbs() {
        let product = Wide::mul_u128(u128::MAX, u128::MAX).unwrap();
        assert_eq!(product.limbs, [1, 0, u64::MAX - 1, u64::MAX]);
    }

    #[test]
    fn rational_service_reports_only_true_overservice_as_exhaustion() {
        let mut request = Request::new(RequestClass::Normal, 1, 8, 0, 0).unwrap();
        let k = (u128::MAX - (ONE - 1)) / ONE;
        request.set_fraction(k + ONE, k * ONE).unwrap();
        let oversized_work = (u128::MAX / NICE_0) * NICE_0;
        assert_eq!(
            request.consume_work(oversized_work),
            Err(ModelError::InvalidState)
        );
        request.consume_work_up_to(oversized_work).unwrap();
        assert!(request.is_exhausted());
    }

    #[test]
    fn repeated_coprime_fraction_reconfigure_and_wake_stays_bounded() {
        let mut entity = Entity::new(RequestClass::Normal, 3, 8, 0).unwrap();
        let mut clock = Clock::new(8);
        for _ in 0..64 {
            entity
                .fractional_service(&mut clock, (ONE / 3) as u64)
                .unwrap();
            entity
                .reconfigure(RequestClass::Normal, 5, 8, clock.v)
                .unwrap();
            entity.begin_sleep(clock.v).unwrap();
            entity.wake_preserving_progress(clock.v + GRACE).unwrap();
            entity
                .reconfigure(RequestClass::Normal, 7, 8, clock.v)
                .unwrap();
            entity
                .reconfigure(RequestClass::Normal, 3, 8, clock.v)
                .unwrap();
        }
        assert!(entity.request.remaining_fraction_den > 0);
        assert!(entity.request.remaining_fraction_num <= entity.request.remaining_fraction_den);
    }

    #[test]
    fn fractional_runtime_conversion_conserves_q32_service() {
        let period = 10u128;
        let samples = [3u128, 7, 2, 8, 4, 6];
        let mut chunked = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut chunked_clock = Clock::new(NICE_0);
        let mut conversion_remainder = 0u128;
        let mut service_q = 0u128;
        let mut elapsed = 0u128;
        for sample in samples {
            elapsed += sample;
            let numerator = sample * ONE + conversion_remainder;
            let sample_q = numerator / period;
            service_q += sample_q;
            conversion_remainder = numerator % period;
            let whole_ticks = sample_q / ONE;
            if whole_ticks != 0 {
                chunked
                    .bulk_service(&mut chunked_clock, whole_ticks)
                    .unwrap();
            }
            let fractional_q = (sample_q % ONE) as u64;
            if fractional_q != 0 {
                chunked
                    .fractional_service(&mut chunked_clock, fractional_q)
                    .unwrap();
            }
        }
        assert_eq!(service_q, (elapsed / period) * ONE);
        assert_eq!(conversion_remainder, (elapsed % period) * ONE % period);

        // The scheduler-side split is equivalent to one exact fixed-width
        // division, independent of how ownership boundaries partition the
        // same elapsed interval.
        let (whole, remainder) = checked_mul_div_rem(elapsed, ONE, period).unwrap();
        assert_eq!(service_q, whole);
        assert_eq!(conversion_remainder, remainder);

        let mut whole_entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut whole_clock = Clock::new(NICE_0);
        whole_entity
            .bulk_service(&mut whole_clock, whole / ONE)
            .unwrap();
        let whole_fractional = (whole % ONE) as u64;
        if whole_fractional != 0 {
            whole_entity
                .fractional_service(&mut whole_clock, whole_fractional)
                .unwrap();
        }
        assert_eq!(chunked, whole_entity);
        assert_eq!(chunked_clock.v, whole_clock.v);
        assert_eq!(chunked_clock.remainder, whole_clock.remainder);
    }

    #[test]
    fn empty_clock_accepts_zero_and_checked_weight_aggregates() {
        let mut clock = Clock::new(0);
        assert_eq!(clock.work_balance(), Ok(0));
        assert_eq!(clock.advance_ticks(0), Ok(0));
        let before = clock;
        assert_eq!(clock.advance_ticks(1), Err(ModelError::InvalidWeight));
        assert_eq!(clock, before);

        clock.checked_add_weight(3).unwrap();
        assert_eq!(clock.total_weight, 3);
        assert_eq!(clock.remainder, 0);
        clock.advance_ticks(1).unwrap();
        assert_ne!(clock.remainder, 0);
        clock.checked_sub_weight(3).unwrap();
        assert_eq!(clock.total_weight, 0);
        assert_eq!(clock.remainder, 0);
        assert_eq!(clock.advance_ticks(1), Err(ModelError::InvalidWeight));
    }

    #[test]
    fn nonempty_clock_weight_rebase_rounds_and_round_trips() {
        let mut clock = Clock::with_parts(11, 2, 3, 7);
        clock.set_total_weight(5).unwrap();
        assert_eq!(clock.v, 11);
        assert_eq!(clock.remainder, 3); // round(2 * 5 / 3)
        assert_eq!(clock.total_weight, 5);

        clock.set_total_weight(3).unwrap();
        assert_eq!(clock.v, 11);
        assert_eq!(clock.remainder, 2); // round(3 * 3 / 5)

        // Exact-half cases use ties-to-even.
        clock.remainder = 2;
        clock.total_weight = 4;
        clock.set_total_weight(3).unwrap(); // 6/4 = 1.5 -> 2
        assert_eq!(clock.remainder, 2);
        clock.remainder = 2;
        clock.total_weight = 4;
        clock.set_total_weight(5).unwrap(); // 10/4 = 2.5 -> 2
        assert_eq!(clock.remainder, 2);
    }

    #[test]
    fn clock_weight_rebase_promotes_rounded_denominator_boundary() {
        let mut clock = Clock::with_parts(7, 2, 3, 0);
        clock.set_total_weight(1).unwrap(); // round(2 / 3) == 1
        assert_eq!(clock.v, 8);
        assert_eq!(clock.remainder, 0);
        clock.set_total_weight(3).unwrap();
        assert_eq!(clock.v, 8);
        assert_eq!(clock.remainder, 0);
    }

    #[test]
    fn no_eligible_jump_preserves_remainder_and_ticks() {
        let entity =
            Entity::with_lag(RequestClass::Normal, NICE_0, NICE_0, 0, -(WORK as i128)).unwrap();
        let mut clock = Clock::with_parts(0, 17, NICE_0, 9);
        let before = clock;
        clock.jump_to(entity.eligible_at().unwrap()).unwrap();
        assert_eq!(clock.remainder, before.remainder);
        assert_eq!(clock.accounted_ticks, before.accounted_ticks);
    }

    #[test]
    fn service_advances_clock_and_request_remaining_atomically() {
        let mut e = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut clock = Clock::new(NICE_0);
        e.tick(&mut clock).unwrap();
        assert_eq!(e.request.remaining_ticks, TARGET_TICKS_NORMAL - 1);
        assert_eq!(clock.v, ONE as i128);
    }

    #[test]
    fn bulk_service_accounts_maximum_elapsed_ticks_and_preserves_lag_debt() {
        let ticks = u64::MAX as u128;
        let weight = NICE_0;
        let total_weight = 2 * NICE_0;
        let mut entity = Entity::new(RequestClass::Normal, weight, total_weight, 0).unwrap();
        let mut clock = Clock::new(total_weight);

        entity.bulk_service(&mut clock, ticks).unwrap();

        assert_eq!(clock.accounted_ticks, ticks);
        assert_eq!(entity.request.remaining(), 0);
        assert_eq!(entity.lag_stamp, clock.v);
        let expected_lag = clock.v * weight as i128 - (ticks * WORK) as i128;
        assert_eq!(entity.lag, expected_lag);
        assert!(entity.lag < 0);
    }

    #[test]
    fn bulk_service_matches_tick_service_before_request_boundary() {
        let ticks = 3;
        let mut bulk = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut ticked = bulk;
        let mut bulk_clock = Clock::new(NICE_0);
        let mut tick_clock = Clock::new(NICE_0);

        bulk.bulk_service(&mut bulk_clock, ticks).unwrap();
        for _ in 0..ticks {
            ticked.tick(&mut tick_clock).unwrap();
        }

        assert_eq!(bulk, ticked);
        assert_eq!(bulk_clock, tick_clock);
    }

    #[test]
    fn yield_chains_start_from_max_old_deadline_and_preempt_is_exact() {
        let mut e = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        e.tick(&mut Clock::new(NICE_0)).unwrap();
        let old = e.request;
        e.yield_request(NICE_0, 0).unwrap();
        assert_eq!(e.request.start, old.deadline);
        let yielded = e.request;
        e.yield_request(NICE_0, 0).unwrap();
        assert_eq!(e.request.start, yielded.deadline);
        let before = e.request;
        e.preempt_request().unwrap();
        assert_eq!(e.request, before);
    }

    #[test]
    fn sleeper_grace_linear_endpoints_and_repeated_short_sleep() {
        let mut e = Entity::with_lag(RequestClass::Normal, NICE_0, NICE_0, 0, -1000).unwrap();
        e.sleep(0).unwrap();
        e.wake(GRACE / 2).unwrap();
        assert_eq!(e.lag, -1000);
        e.sleep(GRACE / 2).unwrap();
        e.wake(GRACE).unwrap();
        assert_eq!(e.lag, -1000);

        let lag = 640i128;
        let halfway =
            bounded_sleeper_decay(RequestClass::Normal, lag, GRACE + DECAY_WINDOW / 2).unwrap();
        assert_eq!(halfway, 320);
        assert_eq!(
            bounded_sleeper_decay(RequestClass::Normal, lag, GRACE + DECAY_WINDOW).unwrap(),
            0
        );
        let cap = bounded_sleeper_decay(RequestClass::Normal, i128::MAX, GRACE / 2).unwrap();
        assert_eq!(cap, credit_cap(RequestClass::Normal));
    }

    #[test]
    fn migration_preserves_lag_offsets_and_remaining_request() {
        let mut e = Entity::new(RequestClass::Batch, NICE_0, NICE_0, 0).unwrap();
        let mut source_clock = Clock::new(NICE_0);
        e.tick_service(&mut source_clock, 3).unwrap();
        let source_v = 11i128;
        let snapshot = e.migration_snapshot(source_v).unwrap();
        let destination_v = -19i128;
        let rebuilt = Entity::from_migration(snapshot, destination_v).unwrap();
        assert_eq!(rebuilt.lag, snapshot.lag);
        assert_eq!(
            rebuilt.request.remaining_ticks,
            snapshot.request.remaining_ticks
        );
        assert_eq!(
            rebuilt.request.deadline - destination_v,
            snapshot.request.deadline - source_v
        );
        assert_eq!(
            rebuilt.request.start - destination_v,
            snapshot.request.start - source_v
        );
    }

    #[test]
    fn reweight_preserves_lag_and_remaining_with_new_virtual_span() {
        let mut e = Entity::new(RequestClass::Batch, NICE_0, NICE_0, 0).unwrap();
        let mut clock = Clock::new(NICE_0);
        e.tick_service(&mut clock, 5).unwrap();
        let old_remaining = e.request.remaining_ticks;
        let old_lag = e.lag_at(clock.v).unwrap();
        e.reweight(2 * NICE_0, 2 * NICE_0, clock.v).unwrap();
        assert_eq!(e.lag, old_lag);
        assert_eq!(e.request.remaining_ticks, old_remaining);
        assert_eq!(
            e.request.deadline - e.request.start,
            checked_virtual_length(old_remaining, 2 * NICE_0).unwrap()
        );
    }

    #[test]
    fn reweight_rescales_active_request_position_when_quantum_changes() {
        let mut e = Entity::new(RequestClass::Batch, 1, 8, 0).unwrap();
        let old_q = e.request.q;
        let consumed = old_q / 2;
        let new_q = request_quantum(RequestClass::Batch, 2, 8).unwrap();
        e.request.consume(consumed).unwrap();
        e.reweight(2, 8, 0).unwrap();
        assert_eq!(e.request.q, new_q);
        let expected_remaining = checked_mul_div_ceil(old_q - consumed, new_q, old_q).unwrap();
        assert_eq!(e.request.remaining_ticks, expected_remaining);
        assert_eq!(
            e.request.deadline - e.request.start,
            checked_virtual_length(expected_remaining, 2).unwrap()
        );

        let mut e = Entity::new(RequestClass::Batch, 2, 8, 0).unwrap();
        let old_q = e.request.q;
        let consumed = old_q / 2;
        let new_q = request_quantum(RequestClass::Batch, 1, 8).unwrap();
        e.request.consume(consumed).unwrap();
        e.reweight(1, 8, 0).unwrap();
        assert_eq!(e.request.q, new_q);
        let expected_remaining = checked_mul_div_ceil(old_q - consumed, new_q, old_q).unwrap();
        assert_eq!(e.request.remaining_ticks, expected_remaining);
        assert_eq!(
            e.request.deadline - e.request.start,
            checked_virtual_length(expected_remaining, 1).unwrap()
        );

        let mut finished = Entity::new(RequestClass::Batch, 1, 8, 0).unwrap();
        finished.request.consume(finished.request.q).unwrap();
        finished.reweight(2, 8, 0).unwrap();
        assert_eq!(finished.request.remaining_ticks, 0);
        assert_eq!(finished.request.deadline, finished.request.start);
    }

    #[test]
    fn reconfigure_normal_batch_idle_preserves_lag_and_request_progress() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        entity.request.consume(3).unwrap();
        let old_q = entity.request.q;
        let old_remaining = entity.request.remaining_ticks;
        let old_lag = entity.lag;

        entity
            .reconfigure(RequestClass::Batch, NICE_0, NICE_0, 0)
            .unwrap();
        assert_eq!(entity.class, RequestClass::Batch);
        assert_eq!(entity.lag, old_lag);
        assert_eq!(
            entity.request.remaining_ticks,
            checked_mul_div_ceil(old_remaining, TARGET_TICKS_BATCH, old_q).unwrap()
        );
        assert_eq!(entity.request.class, RequestClass::Batch);

        let batch_remaining = entity.request.remaining_ticks;
        entity
            .reconfigure(RequestClass::Idle, NICE_0, NICE_0, 0)
            .unwrap();
        assert_eq!(entity.class, RequestClass::Idle);
        assert_eq!(entity.lag, old_lag);
        assert_eq!(
            entity.request.remaining_ticks,
            checked_mul_div_ceil(batch_remaining, TARGET_TICKS_IDLE, TARGET_TICKS_BATCH).unwrap()
        );
        assert_eq!(entity.request.class, RequestClass::Idle);
        assert_eq!(
            entity.request.deadline - entity.request.start,
            checked_virtual_length(entity.request.remaining_ticks, NICE_0).unwrap()
        );
    }

    #[test]
    fn repeated_reconfigure_round_trip_keeps_exact_fractional_progress() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let original_q = entity.request.q;
        entity.request.consume(1).unwrap();
        let remaining_fraction = (
            entity.request.remaining_fraction_num,
            entity.request.remaining_fraction_den,
        );

        for _ in 0..32 {
            entity
                .reconfigure(RequestClass::Batch, NICE_0, NICE_0, 0)
                .unwrap();
            entity
                .reconfigure(RequestClass::Normal, NICE_0, NICE_0, 0)
                .unwrap();
        }

        assert_eq!(entity.request.q, original_q);
        assert_eq!(
            (
                entity.request.remaining_fraction_num,
                entity.request.remaining_fraction_den
            ),
            remaining_fraction
        );
        assert_eq!(entity.request.remaining_ticks, original_q - 1);
    }

    #[test]
    fn combined_whole_and_fractional_service_is_failure_atomic() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let mut clock = Clock::with_parts(i128::MAX, 0, NICE_0, 9);
        let before_entity = entity;
        let before_clock = clock;
        assert_eq!(
            entity.service(&mut clock, 1, ONE / 2),
            Err(ModelError::ArithmeticExhausted)
        );
        assert_eq!(entity, before_entity);
        assert_eq!(clock, before_clock);
    }

    #[test]
    fn failed_reconfigure_is_atomic() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        entity.request.consume(2).unwrap();
        let before = entity;
        assert_eq!(
            entity.reconfigure(RequestClass::Batch, 0, NICE_0, 0),
            Err(ModelError::InvalidWeight)
        );
        assert_eq!(entity, before);
    }

    #[test]
    fn failed_operations_are_atomic() {
        let mut clock = Clock::with_parts(i128::MAX, 0, NICE_0, 0);
        let before_clock = clock;
        assert_eq!(clock.advance_ticks(1), Err(ModelError::ArithmeticExhausted));
        assert_eq!(clock, before_clock);

        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        let before_entity = entity;
        assert_eq!(
            entity.reweight(0, NICE_0, 0),
            Err(ModelError::InvalidWeight)
        );
        assert_eq!(entity, before_entity);
        assert_eq!(entity.wake(0), Err(ModelError::InvalidState));
        assert_eq!(entity, before_entity);

        let mut overflow_rebase = Clock::with_parts(i128::MAX, 2, 3, 0);
        let before_rebase = overflow_rebase;
        assert_eq!(
            overflow_rebase.set_total_weight(1),
            Err(ModelError::ArithmeticExhausted)
        );
        assert_eq!(overflow_rebase, before_rebase);
    }

    #[test]
    fn frozen_reconfigure_does_not_charge_elapsed_rt_virtual_time() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        entity.request.consume(3).unwrap();
        let remaining = entity.request.remaining_ticks;
        entity.freeze_at(0).unwrap();
        entity
            .reconfigure_frozen(RequestClass::Batch, NICE_0, NICE_0, 17 * ONE as i128)
            .unwrap();
        assert_eq!(entity.lag, 0);
        assert_eq!(entity.lag_stamp, 17 * ONE as i128);
        assert_eq!(entity.request.remaining_ticks, remaining * 4);
    }

    #[test]
    fn frozen_sleep_end_has_no_sleeper_credit_or_rt_lag() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, NICE_0, 0).unwrap();
        entity.begin_sleep(0).unwrap();
        let lag = entity.lag;
        entity.end_sleep_frozen(19 * ONE as i128).unwrap();
        assert!(!entity.is_sleeping());
        assert_eq!(entity.lag, lag);
        assert_eq!(entity.lag_at(19 * ONE as i128).unwrap(), lag);
    }

    #[test]
    fn wake_preserving_progress_reanchors_without_rescaling() {
        let mut entity = Entity::new(RequestClass::Normal, NICE_0, 2 * NICE_0, 0).unwrap();
        entity.request.consume(2).unwrap();
        let request = entity.request;
        entity.begin_sleep(0).unwrap();
        entity.wake_preserving_progress(GRACE / 2).unwrap();
        assert_eq!(entity.request.q, request.q);
        assert_eq!(entity.request.remaining_ticks, request.remaining_ticks);
        assert!(!entity.is_sleeping());
    }

    #[test]
    fn deterministic_full_scan_reference_trace() {
        let mut entities = [
            Entity::new(RequestClass::Normal, NICE_0, 3 * NICE_0, 0).unwrap(),
            Entity::new(RequestClass::Batch, NICE_0, 3 * NICE_0, 0).unwrap(),
            Entity::with_lag(RequestClass::Idle, NICE_0, 3 * NICE_0, 0, -7).unwrap(),
        ];
        let mut clock = Clock::new(3 * NICE_0);
        let mut seed = 0x9e37_79b9_u64;
        for _ in 0..48 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = seed as usize % entities.len();
            let frontier = min_eligible_at(&entities).unwrap().unwrap();
            if clock.v < frontier {
                clock.jump_to(frontier).unwrap();
            }
            let scan = entities
                .iter()
                .filter(|entity| entity.is_eligible(clock.v).unwrap())
                .count();
            assert!(scan > 0);
            if entities[index].is_eligible(clock.v).unwrap() {
                if entities[index].request.remaining_ticks == 0 {
                    let total_weight = clock.total_weight;
                    entities[index].renew(total_weight, clock.v).unwrap();
                }
                entities[index].tick(&mut clock).unwrap();
            }
            for entity in &entities {
                assert_eq!(
                    entity.is_eligible(clock.v).unwrap(),
                    entity.lag_at(clock.v).unwrap() >= 0
                );
            }
        }
    }
}
