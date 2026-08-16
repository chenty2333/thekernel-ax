//! Owned x86_64 legacy FXSAVE images and current-task commit boundaries.
//!
//! The image is a copy of the architectural 512-byte FXSAVE memory format. It
//! is deliberately separate from [`axhal::context::TaskContext`]: the task
//! context remains the only owner of the saved image used by context switch,
//! while callers receive only an owned copy. A [`LegacyFxsaveSession`] keeps
//! the current task pinned with IRQs and preemption disabled while the saved
//! image is snapshotted and a validated image is committed.

use core::{arch::x86_64::_fxsave64, fmt, mem::MaybeUninit, ptr::copy_nonoverlapping};

use axhal::context::TaskContext;
use kernel_guard::NoPreemptIrqSave;

/// Size of the legacy FXSAVE/FXRSTOR memory image.
pub const LEGACY_FXSAVE_IMAGE_SIZE: usize = 512;

const FCW_OFFSET: usize = 0;
const FTW_OFFSET: usize = 4;
const MXCSR_OFFSET: usize = 24;
const MXCSR_MASK_OFFSET: usize = 28;
const DEFAULT_MXCSR: u32 = 0x1f80;
const DEFAULT_MXCSR_MASK: u32 = 0xffbf;

/// An owned, copyable legacy FXSAVE image.
///
/// The representation is exactly 512 bytes and is 16-byte aligned as required
/// by FXSAVE and FXRSTOR. The bytes are private so callers cannot create a
/// mutable alias to the task's saved context; use [`Self::from_bytes`] to
/// construct a candidate image.
#[repr(C, align(16))]
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LegacyFxsaveImage {
    bytes: [u8; LEGACY_FXSAVE_IMAGE_SIZE],
}

const _: () = assert!(core::mem::size_of::<LegacyFxsaveImage>() == 512);
const _: () = assert!(core::mem::align_of::<LegacyFxsaveImage>() == 16);

impl LegacyFxsaveImage {
    /// Constructs an image from an owned byte array.
    pub const fn from_bytes(bytes: [u8; LEGACY_FXSAVE_IMAGE_SIZE]) -> Self {
        Self { bytes }
    }

    /// Returns an owned copy of the image bytes.
    pub const fn into_bytes(self) -> [u8; LEGACY_FXSAVE_IMAGE_SIZE] {
        self.bytes
    }

    /// Returns the image bytes without exposing mutable access.
    pub const fn as_bytes(&self) -> &[u8; LEGACY_FXSAVE_IMAGE_SIZE] {
        &self.bytes
    }

    /// Returns the image's MXCSR value.
    pub const fn mxcsr(&self) -> u32 {
        read_u32(&self.bytes, MXCSR_OFFSET)
    }

    /// Returns the image's saved MXCSR mask field.
    ///
    /// FXRSTOR does not use this field to decide which MXCSR bits are legal;
    /// validation compares [`Self::mxcsr`] with the mask reported by the CPU.
    pub const fn mxcsr_mask(&self) -> u32 {
        read_u32(&self.bytes, MXCSR_MASK_OFFSET)
    }

    /// Builds the architectural reset image used for a new user execution
    /// context.
    pub const fn reset() -> Self {
        let mut bytes = [0; LEGACY_FXSAVE_IMAGE_SIZE];
        write_u16(&mut bytes, FCW_OFFSET, 0x037f);
        // FXSAVE's FTW is the abridged tag word. Zero means all x87 registers
        // are empty, which is the FNINIT-equivalent state.
        write_u16(&mut bytes, FTW_OFFSET, 0);
        write_u32(&mut bytes, MXCSR_OFFSET, DEFAULT_MXCSR);
        write_u32(&mut bytes, MXCSR_MASK_OFFSET, DEFAULT_MXCSR_MASK);
        Self { bytes }
    }

    /// Validates the image against the current CPU's FXSAVE capabilities.
    ///
    /// Validation uses a separate, aligned scratch image and therefore does
    /// not modify the current task's saved image or the live FPU/SIMD state.
    /// A successful result is a commit token; consuming that token through a
    /// [`LegacyFxsaveSession`] performs no fallible work.
    pub fn validate(self) -> Result<ValidatedLegacyFxsaveImage, LegacyFxsaveImageError> {
        validate_image(self)
    }
}

impl fmt::Debug for LegacyFxsaveImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacyFxsaveImage")
            .field("mxcsr", &format_args!("{:#x}", self.mxcsr()))
            .field("mxcsr_mask", &format_args!("{:#x}", self.mxcsr_mask()))
            .finish()
    }
}

/// A validated image which may be committed without another fallible step.
///
/// The image is not borrowed from a task and cannot alias the task context.
/// The token is consumed by [`LegacyFxsaveSession::commit`].
#[must_use = "commit the validated image or explicitly drop it"]
#[derive(Debug)]
pub struct ValidatedLegacyFxsaveImage {
    image: LegacyFxsaveImage,
}

impl ValidatedLegacyFxsaveImage {
    /// Returns the owned image represented by this token.
    pub const fn image(&self) -> &LegacyFxsaveImage {
        &self.image
    }

    /// Consumes the token and returns the owned image.
    pub const fn into_image(self) -> LegacyFxsaveImage {
        self.image
    }
}

/// Validation failure for a user-provided legacy FXSAVE image.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LegacyFxsaveImageError {
    /// The image's MXCSR contains a bit not supported by this CPU.
    MxcsrUnsupportedBits { mxcsr: u32, supported_mask: u32 },
}

impl fmt::Display for LegacyFxsaveImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MxcsrUnsupportedBits {
                mxcsr,
                supported_mask,
            } => write!(
                formatter,
                "MXCSR {mxcsr:#x} has bits outside CPU mask {supported_mask:#x}"
            ),
        }
    }
}

/// Error returned when a task handle is no longer the current task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LegacyFxsaveTaskError {
    /// The handle was retained across a context switch and cannot safely touch
    /// the live FPU/SIMD state.
    NotCurrent,
}

impl fmt::Display for LegacyFxsaveTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCurrent => formatter.write_str("task is not current on this CPU"),
        }
    }
}

/// A current-task critical section for legacy FXSAVE state operations.
///
/// Construct this through [`crate::CurrentTask::legacy_fxsave_session`]. The
/// session owns the IRQ/preemption guard, so it is impossible for a safe caller
/// to retain a task-context `&mut` while another task runs. Keep the session
/// short and do not block while it is held.
pub struct LegacyFxsaveSession {
    _guard: NoPreemptIrqSave,
    ctx: *mut TaskContext,
}

impl LegacyFxsaveSession {
    pub(crate) fn new(guard: NoPreemptIrqSave, ctx: *mut TaskContext) -> Self {
        Self { _guard: guard, ctx }
    }

    /// Snapshots live FPU/SIMD state into the task's saved context, then copies
    /// that saved image into an owned value.
    pub fn snapshot(&mut self) -> LegacyFxsaveImage {
        // SAFETY: construction checked that `ctx` belongs to the current task;
        // `_guard` keeps IRQs and preemption disabled until this session ends.
        unsafe { snapshot_context(self.ctx) }
    }

    /// Validates an owned image without changing live or saved task state.
    pub fn validate(
        &self,
        image: LegacyFxsaveImage,
    ) -> Result<ValidatedLegacyFxsaveImage, LegacyFxsaveImageError> {
        image.validate()
    }

    /// Commits a validated image to the saved context and restores it to the
    /// live FPU/SIMD registers.
    ///
    /// The token was validated before this call, and the session's guard keeps
    /// the task current. Consequently this operation has no failure result.
    pub fn commit(self, token: ValidatedLegacyFxsaveImage) {
        // SAFETY: the session guard pins the current task; validation completed
        // all fallible checks before this token was created.
        unsafe { commit_context(self.ctx, token) }
    }

    /// Replaces both the saved and live state with architectural reset values.
    pub fn reset(self) {
        // SAFETY: the reset image has only architecturally valid control bits,
        // and the session guard pins this task as the live owner.
        unsafe { commit_image(self.ctx, LegacyFxsaveImage::reset()) }
    }
}

impl fmt::Debug for LegacyFxsaveSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LegacyFxsaveSession { current task pinned }")
    }
}

const fn read_u32(bytes: &[u8; LEGACY_FXSAVE_IMAGE_SIZE], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

const fn write_u16(bytes: &mut [u8; LEGACY_FXSAVE_IMAGE_SIZE], offset: usize, value: u16) {
    let value = value.to_le_bytes();
    bytes[offset] = value[0];
    bytes[offset + 1] = value[1];
}

const fn write_u32(bytes: &mut [u8; LEGACY_FXSAVE_IMAGE_SIZE], offset: usize, value: u32) {
    let value = value.to_le_bytes();
    bytes[offset] = value[0];
    bytes[offset + 1] = value[1];
    bytes[offset + 2] = value[2];
    bytes[offset + 3] = value[3];
}

fn cpu_mxcsr_mask() -> u32 {
    #[repr(C, align(16))]
    struct Scratch([u8; LEGACY_FXSAVE_IMAGE_SIZE]);

    let mut scratch = MaybeUninit::<Scratch>::zeroed();
    // SAFETY: `Scratch` is 16-byte aligned and exactly 512 bytes. FXSAVE only
    // writes the scratch image; it does not alter the live register state.
    unsafe { _fxsave64(scratch.as_mut_ptr().cast::<u8>()) };
    // SAFETY: FXSAVE initialized all bytes in the scratch image.
    let scratch = unsafe { scratch.assume_init() };
    let mask = read_u32(&scratch.0, MXCSR_MASK_OFFSET);
    effective_mxcsr_mask(mask)
}

const fn effective_mxcsr_mask(mask: u32) -> u32 {
    if mask == 0 { DEFAULT_MXCSR_MASK } else { mask }
}

fn validate_image(
    image: LegacyFxsaveImage,
) -> Result<ValidatedLegacyFxsaveImage, LegacyFxsaveImageError> {
    let supported_mask = cpu_mxcsr_mask();
    let mxcsr = image.mxcsr();
    if mxcsr & !supported_mask != 0 {
        return Err(LegacyFxsaveImageError::MxcsrUnsupportedBits {
            mxcsr,
            supported_mask,
        });
    }
    Ok(ValidatedLegacyFxsaveImage { image })
}

unsafe fn snapshot_context(ctx: *mut TaskContext) -> LegacyFxsaveImage {
    // SAFETY: caller owns the current-task session and therefore has exclusive
    // access to this TaskContext for the duration of the operation.
    unsafe { (*ctx).ext_state.save() };
    let mut bytes = [0; LEGACY_FXSAVE_IMAGE_SIZE];
    // SAFETY: `fxsave_area` is the 16-byte-aligned, 512-byte architectural
    // storage owned by this TaskContext. `bytes` is a distinct destination.
    unsafe {
        copy_nonoverlapping(
            core::ptr::addr_of!((*ctx).ext_state.fxsave_area).cast::<u8>(),
            bytes.as_mut_ptr(),
            LEGACY_FXSAVE_IMAGE_SIZE,
        )
    };
    LegacyFxsaveImage { bytes }
}

#[cfg(test)]
unsafe fn copy_saved_context(ctx: *const TaskContext) -> LegacyFxsaveImage {
    let mut bytes = [0; LEGACY_FXSAVE_IMAGE_SIZE];
    // SAFETY: the caller owns the context for the duration of the copy, and
    // this helper only reads the saved image; it never touches live state.
    unsafe {
        copy_nonoverlapping(
            core::ptr::addr_of!((*ctx).ext_state.fxsave_area).cast::<u8>(),
            bytes.as_mut_ptr(),
            LEGACY_FXSAVE_IMAGE_SIZE,
        )
    };
    LegacyFxsaveImage { bytes }
}

unsafe fn commit_context(ctx: *mut TaskContext, token: ValidatedLegacyFxsaveImage) {
    unsafe { commit_image(ctx, token.image) }
}

unsafe fn commit_image(ctx: *mut TaskContext, image: LegacyFxsaveImage) {
    // SAFETY: the caller holds the session guard; source and destination are
    // distinct owned objects and the destination is exactly the TaskContext's
    // 512-byte FXSAVE area.
    unsafe {
        copy_nonoverlapping(
            image.bytes.as_ptr(),
            core::ptr::addr_of_mut!((*ctx).ext_state.fxsave_area).cast::<u8>(),
            LEGACY_FXSAVE_IMAGE_SIZE,
        );
        (*ctx).ext_state.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_architectural_layout_and_is_copyable() {
        assert_eq!(core::mem::size_of::<LegacyFxsaveImage>(), 512);
        assert_eq!(core::mem::align_of::<LegacyFxsaveImage>(), 16);

        let image = LegacyFxsaveImage::reset();
        let copy = image;
        assert_eq!(copy.mxcsr(), DEFAULT_MXCSR);
        assert_eq!(copy.mxcsr_mask(), DEFAULT_MXCSR_MASK);
        assert_eq!(copy, image);
    }

    #[test]
    fn validation_rejects_reserved_mxcsr_bits_without_touching_image() {
        let mut bytes = LegacyFxsaveImage::reset().into_bytes();
        write_u32(&mut bytes, MXCSR_OFFSET, DEFAULT_MXCSR | (1 << 31));
        let image = LegacyFxsaveImage::from_bytes(bytes);
        let before = image;

        let error = image.validate().unwrap_err();
        assert!(matches!(
            error,
            LegacyFxsaveImageError::MxcsrUnsupportedBits { .. }
        ));
        assert_eq!(image, before);
    }

    #[test]
    fn zero_cpu_mxcsr_mask_uses_architectural_fallback() {
        assert_eq!(effective_mxcsr_mask(0), DEFAULT_MXCSR_MASK);
        assert_eq!(effective_mxcsr_mask(0xffff), 0xffff);
    }

    #[test]
    fn snapshot_and_validated_commit_round_trip_saved_image() {
        let mut context = TaskContext::new();
        // SAFETY: the test owns this freshly-created TaskContext and no live
        // scheduler task can access it concurrently.
        let image = unsafe { snapshot_context(&mut context) };
        let saved_before_validation = unsafe { copy_saved_context(&context) };
        let token = image.validate().expect("CPU must support reset MXCSR");
        let saved_after_validation = unsafe { copy_saved_context(&context) };
        assert_eq!(saved_after_validation, saved_before_validation);
        // SAFETY: same exclusive test ownership as the snapshot above.
        unsafe { commit_context(&mut context, token) };
    }
}
