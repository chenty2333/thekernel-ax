//! Checked integer EEVDF model.
//!
//! The model owns no task reference, intrusive link, tree, queue ownership,
//! real-time class, or task-layer state.  It is deliberately a small value
//! layer which a scheduler can embed in its own allocation-free entity.

pub const FP_SHIFT: u32 = 32;
pub const ONE: u128 = 1u128 << FP_SHIFT;
pub const NICE_0: u128 = 1024;
/// Work represented by one real tick at nice zero.
pub const WORK: u128 = NICE_0 * ONE;

pub const TARGET_TICKS_NORMAL: u128 = 8;
pub const TARGET_TICKS_BATCH: u128 = 32;
pub const TARGET_TICKS_IDLE: u128 = 8;
pub const GRACE: i128 = (8 * ONE) as i128;
pub const DECAY_WINDOW: i128 = (64 * ONE) as i128;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestClass {
    Normal,
    Batch,
    Idle,
}

pub type EntityClass = RequestClass;

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
    let round_up = if remainder > denominator - remainder {
        true
    } else if remainder < denominator - remainder {
        false
    } else {
        // An exact half rounds to the even quotient.
        quotient & 1 != 0
    };
    if round_up {
        quotient
            .checked_add(1)
            .ok_or(ModelError::ArithmeticExhausted)
    } else {
        Ok(quotient)
    }
}

/// Signed mathematical ceil division by a positive unsigned denominator.
pub fn signed_ceil_div(numerator: i128, denominator: u128) -> Result<i128, ModelError> {
    if denominator == 0 {
        return Err(ModelError::InvalidWeight);
    }
    if numerator >= 0 {
        let result = checked_ceil_div(numerator as u128, denominator)?;
        checked_i128(result)
    } else {
        // ceil(-m/d) = -floor(m/d), including i128::MIN.
        let quotient = numerator.unsigned_abs() / denominator;
        if quotient == (1u128 << 127) {
            Ok(i128::MIN)
        } else {
            Ok(-(quotient as i128))
        }
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

/// A current EEVDF request.  `q` is the admitted target and
/// `remaining_ticks` is explicit state, never inferred through saturation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub class: RequestClass,
    pub q: u128,
    pub remaining_ticks: u128,
    pub virtual_length: i128,
    pub start: i128,
    pub deadline: i128,
}

impl Request {
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
        Ok(Self {
            class,
            q,
            remaining_ticks: q,
            virtual_length,
            start,
            deadline,
        })
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

    pub fn consume(&mut self, ticks: u128) -> Result<(), ModelError> {
        if ticks > self.remaining_ticks {
            return Err(ModelError::InvalidState);
        }
        self.remaining_ticks = self
            .remaining_ticks
            .checked_sub(ticks)
            .ok_or(ModelError::ArithmeticExhausted)?;
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

pub fn deadline_after_yield(start: i128, old_deadline: i128, r: i128) -> Result<i128, ModelError> {
    checked_deadline(core::cmp::max(start, old_deadline), r)
}

pub const fn deadline_after_preempt(deadline: i128) -> i128 {
    deadline
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

    pub fn preview_advance(&self, ticks: u128) -> Result<ClockAdvance, ModelError> {
        if ticks == 0 {
            return Ok(ClockAdvance {
                v: self.v,
                remainder: self.remainder,
                accounted_ticks: self.accounted_ticks,
                delta_v: 0,
            });
        }
        if self.total_weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
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
        let (v, remainder) = if rounded == total_weight {
            (
                self.v
                    .checked_add(1)
                    .ok_or(ModelError::ArithmeticExhausted)?,
                0,
            )
        } else if rounded < total_weight {
            (self.v, rounded)
        } else {
            // This is unreachable for a valid residue, but keeping the
            // representation check explicit makes the operation atomic even
            // if a future caller relaxes the invariant above.
            return Err(ModelError::InvalidState);
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

    pub fn tick_service(&mut self, clock: &mut Clock, ticks: u128) -> Result<i128, ModelError> {
        if ticks == 0 {
            return Ok(0);
        }
        if self.weight == 0 {
            return Err(ModelError::InvalidWeight);
        }
        if self.sleeper_v.is_some() {
            return Err(ModelError::InvalidState);
        }
        let next_clock = clock.preview_advance(ticks)?;
        let next_lag = checked_lag_at(self.lag, self.lag_stamp, self.weight, next_clock.v)?
            .checked_sub(checked_work(ticks)?)
            .ok_or(ModelError::ArithmeticExhausted)?;
        let mut next_request = self.request;
        next_request.consume(ticks)?;
        clock.v = next_clock.v;
        clock.remainder = next_clock.remainder;
        clock.accounted_ticks = next_clock.accounted_ticks;
        self.lag = next_lag;
        self.lag_stamp = next_clock.v;
        self.request = next_request;
        Ok(next_clock.delta_v)
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
        if self.weight == 0 || self.request.q == 0 || self.request.remaining_ticks > self.request.q
        {
            return Err(ModelError::InvalidState);
        }
        let lag = bounded_sleeper_decay_inner(self.class, self.lag, elapsed)?;
        let start = request_start(v, lag, self.weight)?;
        let remaining_r = checked_virtual_length(self.request.remaining_ticks, self.weight)?;
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
        if self.request.q == 0 || self.request.remaining_ticks > self.request.q {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let start = request_start(v, lag, self.weight)?;
        let remaining_r = checked_virtual_length(self.request.remaining_ticks, self.weight)?;
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
        let old_q = self.request.q;
        if old_q == 0 || self.request.remaining_ticks > old_q {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag_at(v)?;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let new_remaining = if self.request.remaining_ticks == 0 {
            0
        } else {
            checked_mul_div_ceil(self.request.remaining_ticks, new_q, old_q)?
        };
        if new_remaining > new_q {
            return Err(ModelError::InvalidState);
        }
        let full_r = checked_virtual_length(new_q, new_weight)?;
        let remaining_r = checked_virtual_length(new_remaining, new_weight)?;
        let start = request_start(v, lag, new_weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        let mut request = self.request;
        request.q = new_q;
        request.remaining_ticks = new_remaining;
        request.class = new_class;
        request.virtual_length = full_r;
        request.start = start;
        request.deadline = deadline;
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
        let old_q = self.request.q;
        if old_q == 0 || self.request.remaining_ticks > old_q {
            return Err(ModelError::InvalidState);
        }
        // `self.lag` is deliberately used directly: `lag_at(v)` would charge
        // all virtual time elapsed while the task was in the RT class.
        let lag = self.lag;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let new_remaining = if self.request.remaining_ticks == 0 {
            0
        } else {
            checked_mul_div_ceil(self.request.remaining_ticks, new_q, old_q)?
        };
        if new_remaining > new_q {
            return Err(ModelError::InvalidState);
        }
        let full_r = checked_virtual_length(new_q, new_weight)?;
        let remaining_r = checked_virtual_length(new_remaining, new_weight)?;
        let start = request_start(v, lag, new_weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        let mut request = self.request;
        request.q = new_q;
        request.remaining_ticks = new_remaining;
        request.class = new_class;
        request.virtual_length = full_r;
        request.start = start;
        request.deadline = deadline;
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
        let old_q = self.request.q;
        if old_q == 0 || self.request.remaining_ticks > old_q {
            return Err(ModelError::InvalidState);
        }
        let lag = self.lag;
        let new_q = request_quantum(new_class, new_weight, new_total_weight)?;
        let new_remaining = if self.request.remaining_ticks == 0 {
            0
        } else {
            checked_mul_div_ceil(self.request.remaining_ticks, new_q, old_q)?
        };
        if new_remaining > new_q {
            return Err(ModelError::InvalidState);
        }
        let full_r = checked_virtual_length(new_q, new_weight)?;
        let remaining_r = checked_virtual_length(new_remaining, new_weight)?;
        let start = request_start(sleeper_v, lag, new_weight)?;
        let deadline = checked_deadline(start, remaining_r)?;
        let mut request = self.request;
        request.q = new_q;
        request.remaining_ticks = new_remaining;
        request.class = new_class;
        request.virtual_length = full_r;
        request.start = start;
        request.deadline = deadline;
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
        {
            return Err(ModelError::InvalidState);
        }
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
        assert_eq!(e.request.q, 3); // ceil(8*3/10)
        assert_eq!(e.request.virtual_length, WORK as i128); // ceil(3*WORK/3)
        assert_eq!(e.request.start, 0);
        assert_eq!(e.request.deadline, WORK as i128);

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
        assert_eq!(e.request.remaining_ticks, 7);
        assert_eq!(clock.v, ONE as i128);
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
        assert_eq!(e.request.q, 4);
        e.request.consume(2).unwrap();
        e.reweight(2, 8, 0).unwrap();
        assert_eq!(e.request.q, 8);
        assert_eq!(e.request.remaining_ticks, 4);
        assert_eq!(
            e.request.deadline - e.request.start,
            checked_virtual_length(4, 2).unwrap()
        );

        let mut e = Entity::new(RequestClass::Batch, 2, 8, 0).unwrap();
        assert_eq!(e.request.q, 8);
        e.request.consume(4).unwrap();
        e.reweight(1, 8, 0).unwrap();
        assert_eq!(e.request.q, 4);
        assert_eq!(e.request.remaining_ticks, 2);
        assert_eq!(
            e.request.deadline - e.request.start,
            checked_virtual_length(2, 1).unwrap()
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
