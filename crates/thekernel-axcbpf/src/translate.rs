//! Safe x86_64 classic-BPF machine-code translation.
//!
//! The translator emits a self-contained SysV x86_64 function with the ABI
//! `extern "C" fn(data: *const u8, len: u32) -> u32`. Input loads, arithmetic,
//! scratch accesses, forward branches, and returns are emitted directly; no
//! helper call or address-bearing relocation is needed. The publisher only
//! has to copy the immutable bytes into an executable W^X mapping.

use alloc::vec::Vec;
use core::fmt;

use crate::{
    Ancillary, Input, Instruction, LoadWidth, PacketInputContext, SCRATCH_WORDS, VerifyError,
    ancillary_from_offset, is_ancillary_offset, opcode,
};

/// Maximum bytes in one translated x86_64 image.
pub const MAX_CODE_IMAGE_BYTES: usize = 256 * 1024;

/// Input ABI used by emitted loads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputProfile {
    /// Packet-style byte, halfword, and word loads in network byte order.
    PacketBytesBigEndian,
    /// Packet-style loads plus a typed [`crate::PacketInputContext`] provider.
    ///
    /// The generated entry keeps the existing two-argument native ABI, but
    /// its first argument is a pointer to the context.  The context supplies
    /// the original packet pointer/length and aligned host-order ancillary
    /// metadata.  This avoids copying packet bytes or encoding metadata as a
    /// pseudo-packet prefix.
    PacketContextBigEndian,
    /// Seccomp-style aligned native-endian word loads.
    ///
    /// Alignment is part of the logical input contract: absolute offsets are
    /// verified at translation time and indirect offsets at execution time.
    /// A seccomp adapter must also admit only a four-byte-aligned snapshot
    /// base, while host reference adapters intentionally do not infer this
    /// contract from an incidental slice address.
    NativeAlignedWords,
}

/// Input adapter for [`InputProfile::NativeAlignedWords`] reference tests.
pub struct NativeWordInput<'a> {
    bytes: &'a [u8],
}

impl<'a> NativeWordInput<'a> {
    /// Wraps an immutable native-word input buffer.
    ///
    /// The reference model checks logical offsets, not the address of the
    /// Rust slice. This keeps it equivalent to the JIT's pre-pointer-add
    /// offset check; production seccomp integration must enforce the native
    /// snapshot's four-byte base alignment before invoking the profile.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Wraps a native-word input only when its base address is four-byte
    /// aligned.
    ///
    /// [`NativeWordInput::new`] deliberately keeps the logical-offset
    /// contract independent from an incidental Rust slice address. Adapters
    /// whose native snapshot ABI includes a physical base-alignment
    /// precondition can use this constructor at that boundary instead of
    /// silently losing that part of the contract.
    pub fn new_aligned(bytes: &'a [u8]) -> Option<Self> {
        let input = Self::new(bytes);
        input.base_is_aligned().then_some(input)
    }

    /// Returns whether the wrapped byte view starts at a four-byte boundary.
    ///
    /// This is intentionally separate from the logical offset checks in
    /// [`Input::load`]. x86_64 permits unaligned byte-addressed loads, while
    /// some native-word adapters may require the stronger base-address rule.
    pub fn base_is_aligned(&self) -> bool {
        (self.bytes.as_ptr() as usize) & 3 == 0
    }
}

impl Input for NativeWordInput<'_> {
    fn len(&self) -> u32 {
        u32::try_from(self.bytes.len()).unwrap_or(u32::MAX)
    }

    fn load(&self, offset: u32, width: LoadWidth) -> Option<u32> {
        let offset = usize::try_from(offset).ok()?;
        if matches!(width, LoadWidth::Word) && offset % 4 != 0 {
            return None;
        }
        let end = offset.checked_add(width_bytes(width))?;
        let bytes = self.bytes.get(offset..end)?;
        match width {
            LoadWidth::Byte => Some(u32::from(bytes[0])),
            LoadWidth::Half => Some(u32::from(u16::from_ne_bytes([bytes[0], bytes[1]]))),
            LoadWidth::Word => Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        }
    }
}

/// A call site that a publisher may resolve after image placement.
///
/// The current direct emitter never produces one. The type remains part of
/// the image contract so future non-inline input policies can fail explicitly
/// without changing the image ownership boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum ExternalCall {
    InputLoad { width: LoadWidth },
    InputLength,
}

/// The kind of an image relocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum RelocationKind {
    ExternalCall(ExternalCall),
}

/// A relocation in an immutable [`CodeImage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub struct Relocation {
    pub offset: u32,
    pub kind: RelocationKind,
}

/// Maps one source instruction to emitted machine bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub struct InstructionMap {
    pub source_pc: u32,
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BranchSite {
    source_pc: u32,
    true_pc: u32,
    false_pc: u32,
    true_disp: u32,
    false_disp: u32,
}

/// Failure found by the independent translation validator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum ImageValidationError {
    NoMemory,
    Empty,
    TooLong {
        length: usize,
    },
    UnsupportedOpcode {
        pc: usize,
        code: u16,
    },
    ImmediateDivisionByZero {
        pc: usize,
    },
    ImmediateShiftOutOfRange {
        pc: usize,
        shift: u32,
    },
    ScratchOutOfRange {
        pc: usize,
        index: u32,
    },
    ScratchUninitialized {
        pc: usize,
        index: u32,
    },
    JumpOutOfRange {
        pc: usize,
    },
    UnsupportedAncillaryLoad {
        pc: usize,
        offset: u32,
    },
    MissingFinalReturn,
    InvalidEntry {
        entry: u32,
    },
    InvalidBoundary {
        index: usize,
    },
    BoundaryOutOfBounds {
        index: usize,
    },
    InvalidRecordLength {
        index: usize,
        len: u32,
    },
    SourceMapMismatch {
        index: usize,
    },
    MissingBranchSite {
        pc: usize,
    },
    BranchSiteMismatch {
        pc: usize,
    },
    BranchOutOfBounds {
        pc: usize,
    },
    IndirectControlFlow,
    RelocationOutOfBounds {
        index: usize,
    },
    UnexpectedRelocation {
        index: usize,
    },
    ImageTooLarge {
        size: usize,
    },
    ProfileUnsupported {
        pc: usize,
        code: u16,
    },
    /// The immutable native image contains an instruction outside the
    /// restricted x86_64 subset emitted by this crate.
    NativeDecode {
        offset: usize,
    },
    /// A decoded native instruction does not implement the source operation
    /// at this program counter.
    NativeSemanticMismatch {
        pc: usize,
        offset: usize,
    },
    /// A native direct branch does not target the source block or runtime
    /// failure/epilogue block required by the source program.
    NativeTargetMismatch {
        pc: usize,
    },
    /// Native bytes remain after the independently validated epilogue.
    NativeTrailingBytes {
        offset: usize,
    },
}

impl fmt::Display for ImageValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => formatter.write_str("translation validation allocation failed"),
            Self::Empty => formatter.write_str("translated image is empty"),
            Self::TooLong { length } => {
                write!(formatter, "translated program has {length} instructions")
            }
            Self::UnsupportedOpcode { pc, code } => {
                write!(formatter, "unsupported opcode {code:#x} at {pc}")
            }
            Self::ImmediateDivisionByZero { pc } => {
                write!(formatter, "zero immediate divisor at {pc}")
            }
            Self::ImmediateShiftOutOfRange { pc, shift } => {
                write!(formatter, "shift {shift} out of range at {pc}")
            }
            Self::ScratchOutOfRange { pc, index } => {
                write!(formatter, "scratch {index} out of range at {pc}")
            }
            Self::ScratchUninitialized { pc, index } => {
                write!(formatter, "scratch {index} uninitialized at {pc}")
            }
            Self::JumpOutOfRange { pc } => write!(formatter, "jump out of range at {pc}"),
            Self::UnsupportedAncillaryLoad { pc, offset } => {
                write!(formatter, "unsupported ancillary load {offset:#x} at {pc}")
            }
            Self::MissingFinalReturn => {
                formatter.write_str("translated source has no final return")
            }
            Self::InvalidEntry { entry } => write!(formatter, "invalid entry offset {entry}"),
            Self::InvalidBoundary { index } => write!(formatter, "invalid boundary {index}"),
            Self::BoundaryOutOfBounds { index } => {
                write!(formatter, "boundary {index} out of bounds at {index}")
            }
            Self::InvalidRecordLength { index, len } => {
                write!(formatter, "invalid emitted length {len} at {index}")
            }
            Self::SourceMapMismatch { index } => {
                write!(formatter, "source map mismatch at {index}")
            }
            Self::MissingBranchSite { pc } => write!(formatter, "missing branch metadata at {pc}"),
            Self::BranchSiteMismatch { pc } => {
                write!(formatter, "branch metadata mismatch at {pc}")
            }
            Self::BranchOutOfBounds { pc } => {
                write!(formatter, "branch target out of bounds at {pc}")
            }
            Self::IndirectControlFlow => {
                formatter.write_str("indirect control flow in translated image")
            }
            Self::RelocationOutOfBounds { index } => {
                write!(formatter, "relocation {index} out of bounds")
            }
            Self::UnexpectedRelocation { index } => {
                write!(formatter, "unexpected relocation {index}")
            }
            Self::ImageTooLarge { size } => write!(formatter, "translated image is {size} bytes"),
            Self::ProfileUnsupported { pc, code } => {
                write!(
                    formatter,
                    "opcode {code:#x} is unavailable for the input profile at {pc}"
                )
            }
            Self::NativeDecode { offset } => {
                write!(formatter, "unsupported native instruction at byte {offset}")
            }
            Self::NativeSemanticMismatch { pc, offset } => {
                write!(
                    formatter,
                    "native lowering mismatch at source {pc}, byte {offset}"
                )
            }
            Self::NativeTargetMismatch { pc } => {
                write!(formatter, "native branch target mismatch at source {pc}")
            }
            Self::NativeTrailingBytes { offset } => {
                write!(formatter, "native trailing bytes at byte {offset}")
            }
        }
    }
}

/// Failure while translating a verified program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum TranslationError {
    UnsupportedOpcode { pc: usize, code: u16 },
    InvalidSource(VerifyError),
    NoMemory,
    ImageTooLarge,
    InvalidImage(ImageValidationError),
    ProfileUnsupported { pc: usize, code: u16 },
}

impl fmt::Display for TranslationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOpcode { pc, code } => {
                write!(formatter, "unsupported opcode {code:#x} at {pc}")
            }
            Self::InvalidSource(error) => write!(formatter, "invalid translation source: {error}"),
            Self::NoMemory => formatter.write_str("translation allocation failed"),
            Self::ImageTooLarge => formatter.write_str("translation image is too large"),
            Self::InvalidImage(error) => {
                write!(formatter, "translation validator rejected image: {error}")
            }
            Self::ProfileUnsupported { pc, code } => {
                write!(
                    formatter,
                    "opcode {code:#x} is unavailable for the input profile at {pc}"
                )
            }
        }
    }
}

/// Immutable native x86_64 bytes and translation metadata.
#[derive(Debug)]
pub struct CodeImage {
    bytes: Vec<u8>,
    relocations: Vec<Relocation>,
    entry: u32,
    boundaries: Vec<u32>,
    instruction_map: Vec<InstructionMap>,
    source: Vec<Instruction>,
    branches: Vec<BranchSite>,
    profile: InputProfile,
}

impl CodeImage {
    /// Returns immutable executable bytes. The first byte is the function
    /// entry and the bytes are suitable for a W^X publisher to copy.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns immutable bytes (alias for [`CodeImage::bytes`]).
    pub fn code(&self) -> &[u8] {
        self.bytes()
    }

    /// Returns the native entry offset, always zero for this image format.
    pub const fn entry(&self) -> u32 {
        self.entry
    }

    /// Returns the native entry offset.
    pub const fn entry_offset(&self) -> u32 {
        self.entry
    }

    /// Returns source boundaries including the terminal code length.
    pub fn instruction_boundaries(&self) -> &[u32] {
        &self.boundaries
    }

    /// Returns source-to-native instruction mappings.
    pub fn instruction_map(&self) -> &[InstructionMap] {
        &self.instruction_map
    }

    /// Returns source boundaries (alias for [`CodeImage::instruction_boundaries`]).
    pub fn boundaries(&self) -> &[u32] {
        self.instruction_boundaries()
    }

    /// Returns relocations. Direct images always return an empty slice.
    pub fn relocations(&self) -> &[Relocation] {
        &self.relocations
    }

    /// Returns the input ABI profile used by native load emission.
    pub const fn profile(&self) -> InputProfile {
        self.profile
    }

    /// Returns the input ABI profile (alias for [`CodeImage::profile`]).
    pub const fn input_profile(&self) -> InputProfile {
        self.profile
    }

    /// Returns the number of translated source instructions.
    pub fn len(&self) -> usize {
        self.source.len()
    }

    /// Returns whether the source program is empty.
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// Returns the exact native byte length.
    pub fn image_size(&self) -> usize {
        self.bytes.len()
    }

    /// Returns the page-rounded size required by a publisher.
    pub fn page_aligned_size(&self, page_size: usize) -> Option<usize> {
        if page_size == 0 {
            return None;
        }
        self.bytes
            .len()
            .checked_add(page_size - 1)
            .map(|size| size / page_size * page_size)
    }

    /// Returns the page-rounded mapping upper bound.
    pub fn page_aligned_size_upper_bound(&self, page_size: usize) -> Option<usize> {
        self.page_aligned_size(page_size)
    }

    /// Returns owned bytes and publisher metadata.
    pub fn into_parts(self) -> (Vec<u8>, Vec<Relocation>, u32, Vec<u32>, Vec<InstructionMap>) {
        (
            self.bytes,
            self.relocations,
            self.entry,
            self.boundaries,
            self.instruction_map,
        )
    }

    /// Returns owned native bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Independently validates native code, source map, branches and relocations.
    pub fn validate(&self) -> Result<(), ImageValidationError> {
        TranslationValidator::validate(self)
    }

    /// Evaluates a second safe reference model from the immutable source copy.
    pub fn evaluate<I: Input + ?Sized>(&self, input: &I) -> u32 {
        evaluate_source(&self.source, input)
    }
}

/// Independent validator for native translated images.
pub struct TranslationValidator;

impl TranslationValidator {
    /// Validates source invariants, native boundaries, direct branches, and
    /// the absence of external or indirect control flow.
    pub fn validate(image: &CodeImage) -> Result<(), ImageValidationError> {
        let layout = crate::translation_validate::validate_translation_layout(
            &image.bytes,
            &image.source,
            image.profile,
        )?;
        validate_image(image, &layout)
    }
}

/// Validates a translated image without making pages executable.
pub fn validate_translation(image: &CodeImage) -> Result<(), ImageValidationError> {
    TranslationValidator::validate(image)
}

pub(crate) fn translate_program(
    source: &[Instruction],
    profile: InputProfile,
) -> Result<CodeImage, TranslationError> {
    validate_profile(source, profile)?;
    validate_source(source).map_err(TranslationError::InvalidSource)?;
    for (pc, instruction) in source.iter().copied().enumerate() {
        if !is_supported(instruction.code) {
            return Err(TranslationError::UnsupportedOpcode {
                pc,
                code: instruction.code,
            });
        }
    }

    let mut assembler = Assembler::new(source.len()).map_err(|_| TranslationError::NoMemory)?;
    assembler.emit_prologue(profile);
    let mut map = Vec::new();
    map.try_reserve_exact(source.len())
        .map_err(|_| TranslationError::NoMemory)?;
    let mut boundaries = Vec::new();
    boundaries
        .try_reserve_exact(source.len() + 2)
        .map_err(|_| TranslationError::NoMemory)?;
    // Source byte maps start after the ABI prologue.  Keeping the prologue
    // outside source instruction zero prevents a source map consumer from
    // attributing saved-register or argument setup bytes to user cBPF.
    boundaries
        .push(u32::try_from(assembler.bytes.len()).map_err(|_| TranslationError::ImageTooLarge)?);

    for (pc, instruction) in source.iter().copied().enumerate() {
        let start = assembler.bytes.len();
        assembler.emit_instruction(pc, instruction, profile);
        let end = assembler.bytes.len();
        boundaries.push(u32::try_from(end).map_err(|_| TranslationError::ImageTooLarge)?);
        map.push(InstructionMap {
            source_pc: pc as u32,
            offset: u32::try_from(start).map_err(|_| TranslationError::ImageTooLarge)?,
            len: u32::try_from(end - start).map_err(|_| TranslationError::ImageTooLarge)?,
        });
    }

    let failure = assembler.bytes.len();
    assembler.emit_zero_return();
    let epilogue = assembler.bytes.len();
    assembler.emit_epilogue();
    assembler.resolve(source.len(), &map, failure, epilogue)?;
    if assembler.bytes.len() > MAX_CODE_IMAGE_BYTES {
        return Err(TranslationError::ImageTooLarge);
    }
    boundaries
        .push(u32::try_from(assembler.bytes.len()).map_err(|_| TranslationError::ImageTooLarge)?);

    let mut owned_source = Vec::new();
    owned_source
        .try_reserve_exact(source.len())
        .map_err(|_| TranslationError::NoMemory)?;
    owned_source.extend_from_slice(source);
    let image = CodeImage {
        bytes: assembler.bytes,
        relocations: Vec::new(),
        entry: 0,
        boundaries,
        instruction_map: map,
        source: owned_source,
        branches: assembler.branches,
        profile,
    };
    image.validate().map_err(TranslationError::InvalidImage)?;
    Ok(image)
}

struct Assembler {
    bytes: Vec<u8>,
    patches: Vec<Patch>,
    branches: Vec<BranchSite>,
}

#[derive(Clone, Copy)]
struct Patch {
    disp: usize,
    target: Target,
}

#[derive(Clone, Copy)]
enum Target {
    Pc(usize),
    Failure,
    Epilogue,
}

impl Assembler {
    fn new(source_len: usize) -> Result<Self, alloc::collections::TryReserveError> {
        let mut bytes = Vec::new();
        let byte_capacity = source_len
            .checked_mul(128)
            .and_then(|size| size.checked_add(64))
            .unwrap_or(usize::MAX);
        bytes.try_reserve_exact(byte_capacity)?;
        let mut patches = Vec::new();
        patches.try_reserve_exact(source_len.saturating_mul(8))?;
        let mut branches = Vec::new();
        branches.try_reserve_exact(source_len)?;
        Ok(Self {
            bytes,
            patches,
            branches,
        })
    }

    fn emit_prologue(&mut self, profile: InputProfile) {
        self.bytes.extend_from_slice(&[
            0x55, // push rbp
            0x41, 0x54, // push r12
            0x41, 0x55, // push r13
            0x41, 0x56, // push r14
            0x41, 0x57, // push r15
            0x48, 0x89, 0xe5, // mov rbp,rsp
            0x48, 0x83, 0xec, 0x40, // sub rsp,64
        ]);
        match profile {
            InputProfile::PacketBytesBigEndian | InputProfile::NativeAlignedWords => {
                self.bytes.extend_from_slice(&[
                    0x49, 0x89, 0xfc, // mov r12,rdi (data)
                    0x41, 0x89, 0xf5, // mov r13d,esi (len)
                ]);
            }
            InputProfile::PacketContextBigEndian => {
                // rdi remains the context base for ancillary loads.  r12/r13
                // carry only the original data pointer and packet length.
                self.bytes.extend_from_slice(&[
                    0x4c,
                    0x8b,
                    0x27, // mov r12,[rdi + data]
                    0x44,
                    0x8b,
                    0x6f,
                    PacketInputContext::LEN_OFFSET as u8,
                ]);
            }
        }
        self.bytes.extend_from_slice(&[
            0x31, 0xc0, // xor eax,eax
            0x31, 0xc9, // xor ecx,ecx
        ]);
    }

    fn emit_instruction(&mut self, pc: usize, i: Instruction, profile: InputProfile) {
        match i.code {
            opcode::LD_IMM => self.mov_eax_imm(i.k),
            opcode::LD_W_ABS => self.emit_abs_load(i.k, LoadWidth::Word, profile),
            opcode::LD_H_ABS => self.emit_abs_load(i.k, LoadWidth::Half, profile),
            opcode::LD_B_ABS => self.emit_abs_load(i.k, LoadWidth::Byte, profile),
            opcode::LD_W_IND => self.emit_ind_load(i.k, LoadWidth::Word, profile),
            opcode::LD_H_IND => self.emit_ind_load(i.k, LoadWidth::Half, profile),
            opcode::LD_B_IND => self.emit_ind_load(i.k, LoadWidth::Byte, profile),
            opcode::LD_MEM => self.load_scratch(i.k, false),
            opcode::LD_LEN => self.bytes.extend_from_slice(&[0x44, 0x89, 0xe8]),
            opcode::LDX_IMM => self.mov_ecx_imm(i.k),
            opcode::LDX_MEM => self.load_scratch(i.k, true),
            opcode::LDX_LEN => self.bytes.extend_from_slice(&[0x44, 0x89, 0xe9]),
            opcode::LDX_B_MSH => {
                // BPF_LDX | BPF_B | BPF_MSH writes only X.  Load the nibble
                // directly into ECX so the accumulator survives unchanged.
                self.emit_abs_load_to_x(i.k, profile);
                self.bytes
                    .extend_from_slice(&[0x83, 0xe1, 0x0f, 0xc1, 0xe1, 0x02]);
            }
            opcode::ST => self.store_scratch(i.k, false),
            opcode::STX => self.store_scratch(i.k, true),
            opcode::ALU_ADD_K => self.bin_imm(0x05, i.k),
            opcode::ALU_ADD_X => self.bytes.extend_from_slice(&[0x01, 0xc8]),
            opcode::ALU_SUB_K => self.bin_imm(0x2d, i.k),
            opcode::ALU_SUB_X => self.bytes.extend_from_slice(&[0x29, 0xc8]),
            opcode::ALU_MUL_K => self.imul_imm(i.k),
            opcode::ALU_MUL_X => self.bytes.extend_from_slice(&[0x0f, 0xaf, 0xc1]),
            opcode::ALU_DIV_K => self.div_imm(i.k, false),
            opcode::ALU_DIV_X => self.div_x(false),
            opcode::ALU_OR_K => self.bin_imm(0x0d, i.k),
            opcode::ALU_OR_X => self.bytes.extend_from_slice(&[0x09, 0xc8]),
            opcode::ALU_AND_K => self.bin_imm(0x25, i.k),
            opcode::ALU_AND_X => self.bytes.extend_from_slice(&[0x21, 0xc8]),
            opcode::ALU_LSH_K => self.shift_imm(0xc1, 0xe0, i.k as u8),
            opcode::ALU_LSH_X => self.shift_x(0xd3, 0xe0),
            opcode::ALU_RSH_K => self.shift_imm(0xc1, 0xe8, i.k as u8),
            opcode::ALU_RSH_X => self.shift_x(0xd3, 0xe8),
            opcode::ALU_NEG => self.bytes.extend_from_slice(&[0xf7, 0xd8]),
            opcode::ALU_MOD_K => self.div_imm(i.k, true),
            opcode::ALU_MOD_X => self.div_x(true),
            opcode::ALU_XOR_K => self.bin_imm(0x35, i.k),
            opcode::ALU_XOR_X => self.bytes.extend_from_slice(&[0x31, 0xc8]),
            opcode::JMP_JA => {
                let target = pc + 1 + i.k as usize;
                let disp = self.emit_jump(Target::Pc(target));
                self.branches.push(BranchSite {
                    source_pc: pc as u32,
                    true_pc: target as u32,
                    false_pc: target as u32,
                    true_disp: disp as u32,
                    false_disp: disp as u32,
                });
            }
            opcode::JMP_JEQ_K => self.emit_conditional(0x84, i.k, i, false, pc),
            opcode::JMP_JEQ_X => self.emit_conditional(0x84, 0, i, true, pc),
            opcode::JMP_JGT_K => self.emit_conditional(0x87, i.k, i, false, pc),
            opcode::JMP_JGT_X => self.emit_conditional(0x87, 0, i, true, pc),
            opcode::JMP_JGE_K => self.emit_conditional(0x83, i.k, i, false, pc),
            opcode::JMP_JGE_X => self.emit_conditional(0x83, 0, i, true, pc),
            opcode::JMP_JSET_K => self.emit_conditional(0x85, i.k, i, false, pc),
            opcode::JMP_JSET_X => self.emit_conditional(0x85, 0, i, true, pc),
            opcode::RET_K => {
                self.mov_eax_imm(i.k);
                self.emit_jump(Target::Epilogue);
            }
            opcode::RET_A => {
                self.emit_jump(Target::Epilogue);
            }
            opcode::MISC_TAX => self.bytes.extend_from_slice(&[0x89, 0xc1]),
            opcode::MISC_TXA => self.bytes.extend_from_slice(&[0x89, 0xc8]),
            _ => {}
        }
    }

    fn emit_abs_load(&mut self, offset: u32, width: LoadWidth, profile: InputProfile) {
        if let Some(field) = ancillary_from_offset(offset) {
            debug_assert!(matches!(profile, InputProfile::PacketContextBigEndian));
            self.emit_ancillary_load(field);
            return;
        }
        self.mov_r14d_imm(offset);
        self.emit_bounds(width);
        self.add_r14_r12();
        self.emit_speculation_barrier();
        self.emit_load_r14(width, profile);
    }

    fn emit_abs_load_to_x(&mut self, offset: u32, profile: InputProfile) {
        debug_assert!(ancillary_from_offset(offset).is_none());
        self.mov_r14d_imm(offset);
        self.emit_bounds(LoadWidth::Byte);
        self.add_r14_r12();
        self.emit_speculation_barrier();
        self.emit_load_r14_to_x(profile);
    }

    fn emit_ancillary_load(&mut self, field: Ancillary) {
        // PacketMetadata is eight aligned u32 fields.  One fixed dword load
        // keeps the metadata path branch-free and cache-local; conversions
        // such as protocol byte order are performed by the provider before
        // entering the JIT.
        let offset = PacketInputContext::METADATA_OFFSET + field.metadata_offset();
        debug_assert!(offset <= u8::MAX as usize);
        self.bytes
            .extend_from_slice(&[0x8b, 0x47, u8::try_from(offset).unwrap_or(u8::MAX)]);
    }

    fn emit_ind_load(&mut self, offset: u32, width: LoadWidth, profile: InputProfile) {
        self.bytes.extend_from_slice(&[0x41, 0x89, 0xce]); // mov r14d,ecx
        self.bytes.extend_from_slice(&[0x41, 0x81, 0xc6]);
        self.u32(offset);
        self.jcc(0x82, Target::Failure); // add carry
        self.emit_bounds_ind(width, profile);
        self.add_r14_r12();
        self.emit_speculation_barrier();
        self.emit_load_r14(width, profile);
    }

    fn emit_speculation_barrier(&mut self) {
        // x86_64 LFENCE is the same architectural speculation-stop used by
        // Linux's x86 barrier_nospec() sequence.  Keep it immediately before
        // the packet dereference, after all bounds and pointer-overflow checks.
        self.bytes.extend_from_slice(&[0x0f, 0xae, 0xe8]);
    }

    fn emit_bounds(&mut self, width: LoadWidth) {
        self.cmp_r13_imm(width_bytes(width) as u8);
        self.jcc(0x82, Target::Failure);
        self.bytes.extend_from_slice(&[0x45, 0x89, 0xef]); // mov r15d,r13d
        self.sub_r15_imm(width_bytes(width) as u8);
        self.bytes.extend_from_slice(&[0x45, 0x39, 0xf7]); // cmp r15d,r14d
        self.jcc(0x82, Target::Failure);
    }

    fn emit_bounds_ind(&mut self, width: LoadWidth, profile: InputProfile) {
        self.cmp_r13_imm(width_bytes(width) as u8);
        self.jcc(0x82, Target::Failure);
        self.bytes.extend_from_slice(&[0x45, 0x89, 0xef]);
        self.sub_r15_imm(width_bytes(width) as u8);
        self.bytes.extend_from_slice(&[0x45, 0x39, 0xf7]);
        self.jcc(0x82, Target::Failure);
        if matches!(profile, InputProfile::NativeAlignedWords) && matches!(width, LoadWidth::Word) {
            self.bytes.extend_from_slice(&[0x41, 0xf6, 0xc6, 0x03]);
            self.jcc(0x85, Target::Failure);
        }
    }

    fn add_r14_r12(&mut self) {
        self.bytes.extend_from_slice(&[0x4d, 0x01, 0xe6]);
        self.jcc(0x82, Target::Failure);
    }

    fn emit_load_r14(&mut self, width: LoadWidth, profile: InputProfile) {
        match width {
            LoadWidth::Byte => self.bytes.extend_from_slice(&[0x41, 0x0f, 0xb6, 0x06]),
            LoadWidth::Half => {
                self.bytes
                    // MOVZX r14d, word ptr [r14].  Omitting 0x66 is
                    // intentional: the operand-size override would write
                    // only r14w and leave the previous accumulator's high
                    // sixteen bits in EAX when the result is consumed.
                    .extend_from_slice(&[0x41, 0x0f, 0xb7, 0x06]);
                if matches!(
                    profile,
                    InputProfile::PacketBytesBigEndian | InputProfile::PacketContextBigEndian
                ) {
                    self.bytes.extend_from_slice(&[0x66, 0xc1, 0xc0, 0x08]);
                }
            }
            LoadWidth::Word => {
                self.bytes.extend_from_slice(&[0x41, 0x8b, 0x06]);
                if matches!(
                    profile,
                    InputProfile::PacketBytesBigEndian | InputProfile::PacketContextBigEndian
                ) {
                    self.bytes.extend_from_slice(&[0x0f, 0xc8]);
                }
            }
        }
    }

    fn emit_load_r14_to_x(&mut self, _profile: InputProfile) {
        // MOVZX ecx, byte ptr [r14].  The byte profile is the only one that
        // reaches this helper (NativeAlignedWords rejects LDX_B_MSH).
        self.bytes.extend_from_slice(&[0x41, 0x0f, 0xb6, 0x0e]);
    }

    fn load_scratch(&mut self, index: u32, into_x: bool) {
        let disp = scratch_disp(index);
        self.bytes.push(0x8b);
        self.bytes.push(if into_x { 0x4d } else { 0x45 });
        self.bytes.push(disp);
    }

    fn store_scratch(&mut self, index: u32, from_x: bool) {
        let disp = scratch_disp(index);
        self.bytes.push(0x89);
        self.bytes.push(if from_x { 0x4d } else { 0x45 });
        self.bytes.push(disp);
    }

    fn bin_imm(&mut self, opcode: u8, immediate: u32) {
        self.bytes.push(opcode);
        self.u32(immediate);
    }
    fn imul_imm(&mut self, immediate: u32) {
        self.bytes.extend_from_slice(&[0x69, 0xc0]);
        self.u32(immediate);
    }

    fn div_imm(&mut self, immediate: u32, remainder: bool) {
        self.mov_r14d_imm(immediate);
        self.bytes
            .extend_from_slice(&[0x31, 0xd2, 0x41, 0xf7, 0xf6]);
        if remainder {
            self.bytes.extend_from_slice(&[0x89, 0xd0]);
        }
    }

    fn div_x(&mut self, remainder: bool) {
        self.bytes.extend_from_slice(&[0x85, 0xc9]);
        self.jcc(0x84, Target::Failure);
        self.bytes.extend_from_slice(&[0x31, 0xd2, 0xf7, 0xf1]);
        if remainder {
            self.bytes.extend_from_slice(&[0x89, 0xd0]);
        }
    }

    fn shift_imm(&mut self, opcode: u8, modrm: u8, count: u8) {
        self.bytes.extend_from_slice(&[opcode, modrm, count]);
    }

    fn shift_x(&mut self, opcode: u8, modrm: u8) {
        self.bytes.extend_from_slice(&[
            0x41, 0x89, 0xce, 0x41, 0x83, 0xe6, 0x1f, 0x41, 0x89, 0xcf, 0x44, 0x89, 0xf1, opcode,
            modrm, 0x44, 0x89, 0xf9,
        ]);
    }

    fn emit_conditional(
        &mut self,
        condition: u8,
        immediate: u32,
        i: Instruction,
        x_source: bool,
        pc: usize,
    ) {
        if x_source && condition == 0x85 {
            self.bytes.extend_from_slice(&[0x85, 0xc8]);
        } else if x_source {
            self.bytes.extend_from_slice(&[0x39, 0xc8]);
        } else if condition == 0x85 {
            self.bytes.push(0xa9);
            self.u32(immediate);
        } else {
            self.bytes.push(0x3d);
            self.u32(immediate);
        }
        let true_disp = self.jcc(condition, Target::Pc(pc + 1 + usize::from(i.jt)));
        let false_disp = self.emit_jump(Target::Pc(pc + 1 + usize::from(i.jf)));
        self.branches.push(BranchSite {
            source_pc: pc as u32,
            true_pc: (pc + 1 + usize::from(i.jt)) as u32,
            false_pc: (pc + 1 + usize::from(i.jf)) as u32,
            true_disp: true_disp as u32,
            false_disp: false_disp as u32,
        });
    }

    fn emit_jump(&mut self, target: Target) -> usize {
        self.bytes.push(0xe9);
        let disp = self.bytes.len();
        self.u32(0);
        self.patches.push(Patch { disp, target });
        disp
    }

    fn jcc(&mut self, condition: u8, target: Target) -> usize {
        self.bytes.extend_from_slice(&[0x0f, condition]);
        let disp = self.bytes.len();
        self.u32(0);
        self.patches.push(Patch { disp, target });
        disp
    }

    fn emit_zero_return(&mut self) {
        self.bytes.extend_from_slice(&[0x31, 0xc0]);
        self.emit_jump(Target::Epilogue);
    }

    fn emit_epilogue(&mut self) {
        self.bytes.extend_from_slice(&[
            0x48, 0x83, 0xc4, 0x40, 0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5d, 0xc3,
        ]);
    }

    fn resolve(
        &mut self,
        count: usize,
        map: &[InstructionMap],
        failure: usize,
        epilogue: usize,
    ) -> Result<(), TranslationError> {
        for patch in self.patches.iter().copied() {
            let target = match patch.target {
                Target::Pc(pc) => map
                    .get(pc)
                    .map(|entry| entry.offset as usize)
                    .ok_or(TranslationError::ImageTooLarge)?,
                Target::Failure => failure,
                Target::Epilogue => epilogue,
            };
            let base = patch.disp + 4;
            let displacement =
                i64::try_from(target).unwrap_or(i64::MAX) - i64::try_from(base).unwrap_or(i64::MAX);
            let displacement =
                i32::try_from(displacement).map_err(|_| TranslationError::ImageTooLarge)?;
            self.bytes[patch.disp..patch.disp + 4].copy_from_slice(&displacement.to_le_bytes());
        }
        for branch in &mut self.branches {
            let true_target = map
                .get(branch.true_pc as usize)
                .ok_or(TranslationError::ImageTooLarge)?
                .offset;
            let false_target = map
                .get(branch.false_pc as usize)
                .ok_or(TranslationError::ImageTooLarge)?
                .offset;
            if !patch_matches(&self.bytes, branch.true_disp as usize, true_target as usize)
                || !patch_matches(
                    &self.bytes,
                    branch.false_disp as usize,
                    false_target as usize,
                )
            {
                return Err(TranslationError::ImageTooLarge);
            }
        }
        if count != map.len() {
            return Err(TranslationError::ImageTooLarge);
        }
        Ok(())
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }
    fn mov_eax_imm(&mut self, value: u32) {
        self.bytes.push(0xb8);
        self.u32(value);
    }
    fn mov_ecx_imm(&mut self, value: u32) {
        self.bytes.push(0xb9);
        self.u32(value);
    }
    fn mov_r14d_imm(&mut self, value: u32) {
        self.bytes.extend_from_slice(&[0x41, 0xbe]);
        self.u32(value);
    }
    fn cmp_r13_imm(&mut self, value: u8) {
        self.bytes.extend_from_slice(&[0x41, 0x83, 0xfd, value]);
    }
    fn sub_r15_imm(&mut self, value: u8) {
        self.bytes.extend_from_slice(&[0x41, 0x83, 0xef, value]);
    }
}

fn validate_image(
    image: &CodeImage,
    layout: &crate::translation_validate::TranslationLayout,
) -> Result<(), ImageValidationError> {
    if image.source.is_empty() {
        return Err(ImageValidationError::Empty);
    }
    if image.source.len() > crate::MAX_INSTRUCTIONS {
        return Err(ImageValidationError::TooLong {
            length: image.source.len(),
        });
    }
    if image.entry != 0 {
        return Err(ImageValidationError::InvalidEntry { entry: image.entry });
    }
    if image.bytes.len() > MAX_CODE_IMAGE_BYTES {
        return Err(ImageValidationError::ImageTooLarge {
            size: image.bytes.len(),
        });
    }
    validate_profile_image(&image.source, image.profile)?;
    if let Some((index, relocation)) = image.relocations.iter().enumerate().next() {
        if usize::try_from(relocation.offset)
            .ok()
            .and_then(|offset| offset.checked_add(4))
            .filter(|end| *end <= image.bytes.len())
            .is_none()
        {
            return Err(ImageValidationError::RelocationOutOfBounds { index });
        }
        return Err(ImageValidationError::UnexpectedRelocation { index });
    }
    if image.instruction_map.len() != image.source.len()
        || image.boundaries.len() != image.source.len() + 2
    {
        return Err(ImageValidationError::SourceMapMismatch {
            index: image.instruction_map.len(),
        });
    }
    if image
        .boundaries
        .first()
        .copied()
        .and_then(|bound| usize::try_from(bound).ok())
        .filter(|bound| *bound > image.entry as usize && *bound <= image.bytes.len())
        .is_none()
        || image
            .boundaries
            .last()
            .copied()
            .and_then(|bound| usize::try_from(bound).ok())
            .filter(|bound| *bound == image.bytes.len())
            .is_none()
    {
        return Err(ImageValidationError::BoundaryOutOfBounds { index: 0 });
    }
    if image
        .boundaries
        .windows(2)
        .any(|window| window[0] >= window[1])
    {
        return Err(ImageValidationError::InvalidBoundary { index: 0 });
    }
    for (pc, ((map, source), bounds)) in image
        .instruction_map
        .iter()
        .zip(&image.source)
        .zip(image.boundaries.windows(2).take(image.source.len()))
        .enumerate()
    {
        if map.source_pc != pc as u32
            || map.offset != bounds[0]
            || map.offset != layout.source_offsets[pc]
            || map.len != bounds[1].saturating_sub(bounds[0])
            || map.len == 0
        {
            return Err(ImageValidationError::SourceMapMismatch { index: pc });
        }
        let end = usize::try_from(bounds[1])
            .map_err(|_| ImageValidationError::BoundaryOutOfBounds { index: pc })?;
        if end > image.bytes.len() {
            return Err(ImageValidationError::BoundaryOutOfBounds { index: pc });
        }
        validate_source_instruction(pc, *source)?;
    }
    validate_source(&image.source).map_err(map_verify_error)?;
    for branch in &image.branches {
        let Some(source) = image.source.get(branch.source_pc as usize) else {
            return Err(ImageValidationError::MissingBranchSite {
                pc: branch.source_pc as usize,
            });
        };
        let Some(map) = image.instruction_map.get(branch.source_pc as usize) else {
            return Err(ImageValidationError::MissingBranchSite {
                pc: branch.source_pc as usize,
            });
        };
        let true_offset = image
            .instruction_map
            .get(branch.true_pc as usize)
            .ok_or(ImageValidationError::BranchOutOfBounds {
                pc: branch.source_pc as usize,
            })?
            .offset;
        let false_offset = image
            .instruction_map
            .get(branch.false_pc as usize)
            .ok_or(ImageValidationError::BranchOutOfBounds {
                pc: branch.source_pc as usize,
            })?
            .offset;
        let direct_branch_opcode = if source.code == opcode::JMP_JA {
            branch.true_disp >= 1
                && image.bytes.get(branch.true_disp as usize - 1).copied() == Some(0xe9)
        } else {
            let expected_condition = match source.code {
                opcode::JMP_JEQ_K | opcode::JMP_JEQ_X => 0x84,
                opcode::JMP_JGT_K | opcode::JMP_JGT_X => 0x87,
                opcode::JMP_JGE_K | opcode::JMP_JGE_X => 0x83,
                opcode::JMP_JSET_K | opcode::JMP_JSET_X => 0x85,
                _ => 0,
            };
            branch.true_disp >= 2
                && image.bytes.get(branch.true_disp as usize - 2) == Some(&0x0f)
                && image.bytes.get(branch.true_disp as usize - 1) == Some(&expected_condition)
                && branch.false_disp >= 1
                && image.bytes.get(branch.false_disp as usize - 1) == Some(&0xe9)
        };
        if source.code < opcode::JMP_JA
            || map.offset > branch.true_disp
            || !direct_branch_opcode
            || !patch_matches(
                &image.bytes,
                branch.true_disp as usize,
                true_offset as usize,
            )
            || !patch_matches(
                &image.bytes,
                branch.false_disp as usize,
                false_offset as usize,
            )
        {
            return Err(ImageValidationError::BranchSiteMismatch {
                pc: branch.source_pc as usize,
            });
        }
    }
    let expected_branches = image
        .source
        .iter()
        .filter(|instruction| is_jump(instruction.code))
        .count();
    if expected_branches != image.branches.len()
        || image.branches.iter().enumerate().any(|(index, branch)| {
            image
                .source
                .iter()
                .enumerate()
                .filter(|(_, instruction)| is_jump(instruction.code))
                .nth(index)
                .map(|(pc, _)| branch.source_pc as usize != pc)
                .unwrap_or(true)
        })
    {
        return Err(ImageValidationError::MissingBranchSite { pc: 0 });
    }
    Ok(())
}

fn validate_profile(source: &[Instruction], profile: InputProfile) -> Result<(), TranslationError> {
    for (pc, instruction) in source.iter().copied().enumerate() {
        if matches!(
            instruction.code,
            opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS
        ) && is_ancillary_offset(instruction.k)
            && ancillary_from_offset(instruction.k).is_some()
            && !matches!(profile, InputProfile::PacketContextBigEndian)
        {
            return Err(TranslationError::ProfileUnsupported {
                pc,
                code: instruction.code,
            });
        }
        if let InputProfile::NativeAlignedWords = profile {
            match instruction.code {
                opcode::LD_W_ABS if instruction.k % 4 != 0 => {
                    return Err(TranslationError::ProfileUnsupported {
                        pc,
                        code: instruction.code,
                    });
                }
                opcode::LD_W_IND => {}
                opcode::LD_H_ABS
                | opcode::LD_B_ABS
                | opcode::LD_H_IND
                | opcode::LD_B_IND
                | opcode::LDX_B_MSH => {
                    return Err(TranslationError::ProfileUnsupported {
                        pc,
                        code: instruction.code,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_profile_image(
    source: &[Instruction],
    profile: InputProfile,
) -> Result<(), ImageValidationError> {
    for (pc, instruction) in source.iter().copied().enumerate() {
        if matches!(
            instruction.code,
            opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS
        ) && is_ancillary_offset(instruction.k)
            && ancillary_from_offset(instruction.k).is_some()
            && !matches!(profile, InputProfile::PacketContextBigEndian)
        {
            return Err(ImageValidationError::ProfileUnsupported {
                pc,
                code: instruction.code,
            });
        }
        if let InputProfile::NativeAlignedWords = profile {
            match instruction.code {
                opcode::LD_W_ABS if instruction.k % 4 != 0 => {
                    return Err(ImageValidationError::ProfileUnsupported {
                        pc,
                        code: instruction.code,
                    });
                }
                opcode::LD_W_IND => {}
                opcode::LD_H_ABS
                | opcode::LD_B_ABS
                | opcode::LD_H_IND
                | opcode::LD_B_IND
                | opcode::LDX_B_MSH => {
                    return Err(ImageValidationError::ProfileUnsupported {
                        pc,
                        code: instruction.code,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn patch_matches(bytes: &[u8], disp: usize, target: usize) -> bool {
    let Some(raw) = bytes.get(disp..disp + 4) else {
        return false;
    };
    let displacement = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64;
    i64::try_from(disp + 4)
        .ok()
        .and_then(|base| base.checked_add(displacement))
        .map(|value| value == target as i64)
        .unwrap_or(false)
}

fn scratch_disp(index: u32) -> u8 {
    (-(4_i32 * (index as i32 + 1))) as u8
}

fn width_bytes(width: LoadWidth) -> usize {
    match width {
        LoadWidth::Byte => 1,
        LoadWidth::Half => 2,
        LoadWidth::Word => 4,
    }
}

const fn is_jump(code: u16) -> bool {
    matches!(
        code,
        opcode::JMP_JA
            | opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X
    )
}

fn validate_source_instruction(
    pc: usize,
    instruction: Instruction,
) -> Result<(), ImageValidationError> {
    if !is_supported(instruction.code) {
        return Err(ImageValidationError::UnsupportedOpcode {
            pc,
            code: instruction.code,
        });
    }
    match instruction.code {
        opcode::ALU_DIV_K | opcode::ALU_MOD_K if instruction.k == 0 => {
            Err(ImageValidationError::ImmediateDivisionByZero { pc })
        }
        opcode::ALU_LSH_K | opcode::ALU_RSH_K if instruction.k >= 32 => {
            Err(ImageValidationError::ImmediateShiftOutOfRange {
                pc,
                shift: instruction.k,
            })
        }
        opcode::LD_MEM | opcode::LDX_MEM | opcode::ST | opcode::STX
            if instruction.k >= SCRATCH_WORDS as u32 =>
        {
            Err(ImageValidationError::ScratchOutOfRange {
                pc,
                index: instruction.k,
            })
        }
        opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS
            if instruction.k & 0x8000_0000 != 0
                && ancillary_from_offset(instruction.k).is_none() =>
        {
            Err(ImageValidationError::UnsupportedAncillaryLoad {
                pc,
                offset: instruction.k,
            })
        }
        opcode::LDX_B_MSH if instruction.k & 0x8000_0000 != 0 => {
            Err(ImageValidationError::UnsupportedAncillaryLoad {
                pc,
                offset: instruction.k,
            })
        }
        _ => Ok(()),
    }
}

pub(crate) fn validate_source(instructions: &[Instruction]) -> Result<(), VerifyError> {
    if instructions.is_empty() {
        return Err(VerifyError::Empty);
    }
    if instructions.len() > crate::MAX_INSTRUCTIONS {
        return Err(VerifyError::TooLong {
            length: instructions.len(),
        });
    }
    for (pc, i) in instructions.iter().copied().enumerate() {
        if !is_supported(i.code) {
            return Err(VerifyError::UnsupportedOpcode { pc, code: i.code });
        }
        match i.code {
            opcode::ALU_DIV_K | opcode::ALU_MOD_K if i.k == 0 => {
                return Err(VerifyError::ImmediateDivisionByZero { pc });
            }
            opcode::ALU_LSH_K | opcode::ALU_RSH_K if i.k >= 32 => {
                return Err(VerifyError::ImmediateShiftOutOfRange { pc, shift: i.k });
            }
            opcode::LD_MEM | opcode::LDX_MEM | opcode::ST | opcode::STX
                if i.k >= SCRATCH_WORDS as u32 =>
            {
                return Err(VerifyError::ScratchOutOfRange { pc, index: i.k });
            }
            opcode::JMP_JA => {
                if pc
                    .checked_add(1)
                    .and_then(|next| next.checked_add(i.k as usize))
                    .filter(|target| *target < instructions.len())
                    .is_none()
                {
                    return Err(VerifyError::JumpOutOfRange { pc });
                }
            }
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X => {
                if pc + 1 + usize::from(i.jt) >= instructions.len()
                    || pc + 1 + usize::from(i.jf) >= instructions.len()
                {
                    return Err(VerifyError::JumpOutOfRange { pc });
                }
            }
            opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS
                if i.k & 0x8000_0000 != 0 && ancillary_from_offset(i.k).is_none() =>
            {
                return Err(VerifyError::UnsupportedAncillaryLoad { pc, offset: i.k });
            }
            opcode::LDX_B_MSH if i.k & 0x8000_0000 != 0 => {
                return Err(VerifyError::UnsupportedAncillaryLoad { pc, offset: i.k });
            }
            _ => {}
        }
    }
    if !matches!(
        instructions.last().map(|i| i.code),
        Some(opcode::RET_K | opcode::RET_A)
    ) {
        return Err(VerifyError::MissingFinalReturn);
    }
    let mut incoming = Vec::new();
    incoming
        .try_reserve_exact(instructions.len())
        .map_err(|_| VerifyError::NoMemory)?;
    incoming.resize(instructions.len(), None);
    incoming[0] = Some(0);
    for (pc, i) in instructions.iter().copied().enumerate() {
        let Some(mut initialized) = incoming[pc] else {
            continue;
        };
        match i.code {
            opcode::LD_MEM | opcode::LDX_MEM => {
                let bit = 1_u16 << i.k;
                if initialized & bit == 0 {
                    return Err(VerifyError::ScratchUninitialized { pc, index: i.k });
                }
            }
            opcode::ST | opcode::STX => initialized |= 1_u16 << i.k,
            _ => {}
        }
        match i.code {
            opcode::RET_K | opcode::RET_A => {}
            opcode::JMP_JA => merge(&mut incoming[pc + 1 + i.k as usize], initialized),
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X => {
                merge(&mut incoming[pc + 1 + usize::from(i.jt)], initialized);
                merge(&mut incoming[pc + 1 + usize::from(i.jf)], initialized);
            }
            _ => merge(&mut incoming[pc + 1], initialized),
        }
    }
    Ok(())
}

fn merge(slot: &mut Option<u16>, incoming: u16) {
    *slot = Some(slot.map(|old| old & incoming).unwrap_or(incoming));
}

pub(crate) fn map_verify_error(error: VerifyError) -> ImageValidationError {
    match error {
        VerifyError::Empty => ImageValidationError::Empty,
        VerifyError::TooLong { length } => ImageValidationError::TooLong { length },
        VerifyError::NoMemory => ImageValidationError::NoMemory,
        VerifyError::UnsupportedOpcode { pc, code } => {
            ImageValidationError::UnsupportedOpcode { pc, code }
        }
        VerifyError::UnsupportedAncillaryLoad { pc, offset } => {
            ImageValidationError::UnsupportedAncillaryLoad { pc, offset }
        }
        VerifyError::ImmediateDivisionByZero { pc } => {
            ImageValidationError::ImmediateDivisionByZero { pc }
        }
        VerifyError::ImmediateShiftOutOfRange { pc, shift } => {
            ImageValidationError::ImmediateShiftOutOfRange { pc, shift }
        }
        VerifyError::ScratchOutOfRange { pc, index } => {
            ImageValidationError::ScratchOutOfRange { pc, index }
        }
        VerifyError::ScratchUninitialized { pc, index } => {
            ImageValidationError::ScratchUninitialized { pc, index }
        }
        VerifyError::JumpOutOfRange { pc } => ImageValidationError::JumpOutOfRange { pc },
        VerifyError::MissingFinalReturn => ImageValidationError::MissingFinalReturn,
    }
}

fn evaluate_source<I: Input + ?Sized>(instructions: &[Instruction], input: &I) -> u32 {
    let mut a = 0_u32;
    let mut x = 0_u32;
    let mut scratch = [0_u32; SCRATCH_WORDS];
    let mut pc = 0_usize;
    let mut steps = 0;
    while steps < instructions.len() {
        let i = instructions[pc];
        steps += 1;
        match i.code {
            opcode::LD_IMM => a = i.k,
            opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS => {
                let w = if i.code == opcode::LD_W_ABS {
                    LoadWidth::Word
                } else if i.code == opcode::LD_H_ABS {
                    LoadWidth::Half
                } else {
                    LoadWidth::Byte
                };
                let v = if let Some(field) = ancillary_from_offset(i.k) {
                    input.ancillary(field).unwrap_or(0)
                } else {
                    let Some(v) = input.load(i.k, w) else {
                        return 0;
                    };
                    v
                };
                a = v;
            }
            opcode::LD_W_IND | opcode::LD_H_IND | opcode::LD_B_IND => {
                let Some(offset) = x.checked_add(i.k) else {
                    return 0;
                };
                let w = if i.code == opcode::LD_W_IND {
                    LoadWidth::Word
                } else if i.code == opcode::LD_H_IND {
                    LoadWidth::Half
                } else {
                    LoadWidth::Byte
                };
                let Some(v) = input.load(offset, w) else {
                    return 0;
                };
                a = v;
            }
            opcode::LD_MEM => a = scratch[i.k as usize],
            opcode::LD_LEN => a = input.len(),
            opcode::LDX_IMM => x = i.k,
            opcode::LDX_MEM => x = scratch[i.k as usize],
            opcode::LDX_LEN => x = input.len(),
            opcode::LDX_B_MSH => {
                let Some(v) = input.load(i.k, LoadWidth::Byte) else {
                    return 0;
                };
                x = (v & 0xf) << 2;
            }
            opcode::ST => scratch[i.k as usize] = a,
            opcode::STX => scratch[i.k as usize] = x,
            opcode::ALU_ADD_K => a = a.wrapping_add(i.k),
            opcode::ALU_ADD_X => a = a.wrapping_add(x),
            opcode::ALU_SUB_K => a = a.wrapping_sub(i.k),
            opcode::ALU_SUB_X => a = a.wrapping_sub(x),
            opcode::ALU_MUL_K => a = a.wrapping_mul(i.k),
            opcode::ALU_MUL_X => a = a.wrapping_mul(x),
            opcode::ALU_DIV_K => a /= i.k,
            opcode::ALU_DIV_X => {
                if x == 0 {
                    return 0;
                }
                a /= x;
            }
            opcode::ALU_OR_K => a |= i.k,
            opcode::ALU_OR_X => a |= x,
            opcode::ALU_AND_K => a &= i.k,
            opcode::ALU_AND_X => a &= x,
            opcode::ALU_LSH_K => a = a.wrapping_shl(i.k),
            opcode::ALU_LSH_X => a = a.wrapping_shl(x & 31),
            opcode::ALU_RSH_K => a = a.wrapping_shr(i.k),
            opcode::ALU_RSH_X => a = a.wrapping_shr(x & 31),
            opcode::ALU_NEG => a = a.wrapping_neg(),
            opcode::ALU_MOD_K => a %= i.k,
            opcode::ALU_MOD_X => {
                if x == 0 {
                    return 0;
                }
                a %= x;
            }
            opcode::ALU_XOR_K => a ^= i.k,
            opcode::ALU_XOR_X => a ^= x,
            opcode::JMP_JA => {
                pc += 1 + i.k as usize;
                continue;
            }
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X => {
                let o = if matches!(
                    i.code,
                    opcode::JMP_JEQ_X | opcode::JMP_JGT_X | opcode::JMP_JGE_X | opcode::JMP_JSET_X
                ) {
                    x
                } else {
                    i.k
                };
                let c = match i.code {
                    opcode::JMP_JEQ_K | opcode::JMP_JEQ_X => a == o,
                    opcode::JMP_JGT_K | opcode::JMP_JGT_X => a > o,
                    opcode::JMP_JGE_K | opcode::JMP_JGE_X => a >= o,
                    _ => a & o != 0,
                };
                pc += 1 + usize::from(if c { i.jt } else { i.jf });
                continue;
            }
            opcode::RET_K => return i.k,
            opcode::RET_A => return a,
            opcode::MISC_TAX => x = a,
            opcode::MISC_TXA => a = x,
            _ => return 0,
        }
        pc += 1;
    }
    0
}

const fn is_supported(code: u16) -> bool {
    matches!(
        code,
        opcode::LD_IMM
            | opcode::LD_W_ABS
            | opcode::LD_H_ABS
            | opcode::LD_B_ABS
            | opcode::LD_W_IND
            | opcode::LD_H_IND
            | opcode::LD_B_IND
            | opcode::LD_MEM
            | opcode::LD_LEN
            | opcode::LDX_IMM
            | opcode::LDX_MEM
            | opcode::LDX_LEN
            | opcode::LDX_B_MSH
            | opcode::ST
            | opcode::STX
            | opcode::ALU_ADD_K
            | opcode::ALU_ADD_X
            | opcode::ALU_SUB_K
            | opcode::ALU_SUB_X
            | opcode::ALU_MUL_K
            | opcode::ALU_MUL_X
            | opcode::ALU_DIV_K
            | opcode::ALU_DIV_X
            | opcode::ALU_OR_K
            | opcode::ALU_OR_X
            | opcode::ALU_AND_K
            | opcode::ALU_AND_X
            | opcode::ALU_LSH_K
            | opcode::ALU_LSH_X
            | opcode::ALU_RSH_K
            | opcode::ALU_RSH_X
            | opcode::ALU_NEG
            | opcode::ALU_MOD_K
            | opcode::ALU_MOD_X
            | opcode::ALU_XOR_K
            | opcode::ALU_XOR_X
            | opcode::JMP_JA
            | opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X
            | opcode::RET_K
            | opcode::RET_A
            | opcode::MISC_TAX
            | opcode::MISC_TXA
    )
}
