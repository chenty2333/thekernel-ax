//! Independent validation of the x86_64 cBPF translation.
//!
//! This module deliberately does not consume `CodeImage`'s emitter-produced
//! boundaries, instruction map, or branch metadata.  It decodes the immutable
//! bytes with a small, strict decoder and compares the resulting operations to
//! an independently written source-lowering model.  The decoder is not a
//! general x86 decoder: accepting an encoding outside the translator's
//! closed subset is a validation failure.

use alloc::vec::Vec;

use crate::translate::{ImageValidationError, InputProfile, MAX_CODE_IMAGE_BYTES};
use crate::{
    Instruction, LoadWidth, MAX_INSTRUCTIONS, PacketInputContext, SCRATCH_WORDS,
    ancillary_from_offset, is_ancillary_offset, opcode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Register {
    Eax,
    Ecx,
    Edx,
    Esi,
    R13d,
    R14d,
    R15d,
    Rbp,
    Rsp,
    R12,
    R13,
    R14,
    R15,
    Rdi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Width {
    Byte,
    Half,
    Word,
    Qword,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operand {
    Register(Register),
    Immediate(u32),
    Memory { base: Register, displacement: i8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BinaryKind {
    Add,
    Sub,
    Mul,
    Or,
    And,
    Xor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShiftKind {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnaryKind {
    Neg,
    Bswap,
    RotateLeft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Condition {
    Below,
    Equal,
    NotEqual,
    Above,
    AboveOrEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeOp {
    Push(Register),
    Pop(Register),
    Mov {
        width: Width,
        destination: Operand,
        source: Operand,
    },
    Load {
        width: Width,
        destination: Register,
        base: Register,
        displacement: i8,
    },
    Binary {
        kind: BinaryKind,
        width: Width,
        destination: Operand,
        source: Operand,
    },
    ImmediateMultiply {
        destination: Register,
        source: Register,
        immediate: u32,
    },
    Compare {
        width: Width,
        left: Operand,
        right: Operand,
    },
    Test {
        width: Width,
        left: Operand,
        right: Operand,
    },
    ShiftImmediate {
        kind: ShiftKind,
        width: Width,
        destination: Register,
        count: u8,
    },
    ShiftRegister {
        kind: ShiftKind,
        width: Width,
        destination: Register,
    },
    Unary {
        kind: UnaryKind,
        width: Width,
        destination: Register,
        immediate: u8,
    },
    Divide {
        width: Width,
        divisor: Operand,
    },
    SpeculationBarrier,
    ConditionalJump {
        condition: Condition,
        target: usize,
    },
    Jump {
        target: usize,
    },
    Return,
}

#[derive(Clone, Copy)]
struct Decoded {
    offset: usize,
    operation: NativeOp,
}

#[derive(Clone, Copy)]
enum Target {
    Source(usize),
    Failure,
    Epilogue,
}

#[derive(Clone, Copy)]
struct TargetCheck {
    pc: usize,
    actual: usize,
    expected: Target,
}

/// Independently validates raw x86_64 bytes against a verified cBPF source.
///
/// This is intentionally a separate API from [`crate::validate_translation`]
/// so tests and alternate publishers can validate bytes without constructing
/// or trusting emitter metadata.  The source is validated before decoding;
/// no executable memory or unsafe operation is involved.
pub fn validate_translation_bytes(
    bytes: &[u8],
    source: &[Instruction],
    profile: InputProfile,
) -> Result<(), ImageValidationError> {
    validate_translation_layout(bytes, source, profile).map(|_| ())
}

pub(crate) struct TranslationLayout {
    pub(crate) source_offsets: Vec<u32>,
}

pub(crate) fn validate_translation_layout(
    bytes: &[u8],
    source: &[Instruction],
    profile: InputProfile,
) -> Result<TranslationLayout, ImageValidationError> {
    if bytes.len() > MAX_CODE_IMAGE_BYTES {
        return Err(ImageValidationError::ImageTooLarge { size: bytes.len() });
    }
    // Keep source acceptance here deliberately separate from the translator.
    // The raw-byte validator must not inherit a future widening or regression
    // in the emitter's source helper and then validate the widened image
    // against itself.
    validate_source_contract(source, profile)?;

    let mut checker = Checker::new(bytes);
    checker.validate_prologue(profile)?;

    let mut source_offsets = Vec::new();
    source_offsets
        .try_reserve_exact(source.len())
        .map_err(|_| ImageValidationError::NoMemory)?;
    source_offsets.resize(source.len(), 0);

    let mut targets = Vec::new();
    targets
        .try_reserve_exact(source.len().saturating_mul(8))
        .map_err(|_| ImageValidationError::NoMemory)?;

    for (pc, instruction) in source.iter().copied().enumerate() {
        source_offsets[pc] = u32::try_from(checker.position())
            .map_err(|_| ImageValidationError::ImageTooLarge { size: bytes.len() })?;
        checker.validate_source_instruction(pc, instruction, profile, &mut targets)?;
    }

    let failure_offset = checker.position();
    checker.expect(
        0,
        NativeOp::Binary {
            kind: BinaryKind::Xor,
            width: Width::Word,
            destination: register(Register::Eax),
            source: register(Register::Eax),
        },
    )?;
    let failure_jump = checker.expect_jump(0)?;
    targets.push(TargetCheck {
        pc: source.len() - 1,
        actual: failure_jump,
        expected: Target::Epilogue,
    });

    let epilogue_offset = checker.position();
    checker.validate_epilogue()?;
    if checker.position() != bytes.len() {
        return Err(ImageValidationError::NativeTrailingBytes {
            offset: checker.position(),
        });
    }

    for target in targets {
        let expected = match target.expected {
            Target::Source(pc) => source_offsets
                .get(pc)
                .copied()
                .map(|offset| offset as usize)
                .ok_or(ImageValidationError::NativeTargetMismatch { pc: target.pc })?,
            Target::Failure => failure_offset,
            Target::Epilogue => epilogue_offset,
        };
        if target.actual != expected {
            return Err(ImageValidationError::NativeTargetMismatch { pc: target.pc });
        }
    }
    Ok(TranslationLayout { source_offsets })
}

/// Validates the source contract independently of the emitter.
///
/// This is intentionally a closed copy of the acceptance boundary rather
/// than a call into `translate.rs`: the validator is a second implementation
/// used to detect an emitter that accidentally accepts or lowers a source
/// operation outside the published x86/profile contract.
fn validate_source_contract(
    instructions: &[Instruction],
    profile: InputProfile,
) -> Result<(), ImageValidationError> {
    if instructions.is_empty() {
        return Err(ImageValidationError::Empty);
    }
    if instructions.len() > MAX_INSTRUCTIONS {
        return Err(ImageValidationError::TooLong {
            length: instructions.len(),
        });
    }

    for (pc, instruction) in instructions.iter().copied().enumerate() {
        validate_source_instruction_contract(pc, instruction)?;
        validate_profile_contract(pc, instruction, profile)?;
        match instruction.code {
            opcode::JMP_JA => {
                if source_target(pc, instruction.k as usize, instructions.len()).is_none() {
                    return Err(ImageValidationError::JumpOutOfRange { pc });
                }
            }
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X
                if source_target(pc, usize::from(instruction.jt), instructions.len()).is_none()
                    || source_target(pc, usize::from(instruction.jf), instructions.len())
                        .is_none() =>
            {
                return Err(ImageValidationError::JumpOutOfRange { pc });
            }
            _ => {}
        }
    }

    if !matches!(
        instructions.last().map(|instruction| instruction.code),
        Some(opcode::RET_K | opcode::RET_A)
    ) {
        return Err(ImageValidationError::MissingFinalReturn);
    }
    validate_scratch_initialization(instructions)
}

fn validate_source_instruction_contract(
    pc: usize,
    instruction: Instruction,
) -> Result<(), ImageValidationError> {
    if !source_opcode_supported(instruction.code) {
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

fn validate_profile_contract(
    pc: usize,
    instruction: Instruction,
    profile: InputProfile,
) -> Result<(), ImageValidationError> {
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
    if !matches!(profile, InputProfile::NativeAlignedWords) {
        return Ok(());
    }
    match instruction.code {
        opcode::LD_W_ABS if instruction.k & 3 != 0 => {
            Err(ImageValidationError::ProfileUnsupported {
                pc,
                code: instruction.code,
            })
        }
        opcode::LD_H_ABS
        | opcode::LD_B_ABS
        | opcode::LD_H_IND
        | opcode::LD_B_IND
        | opcode::LDX_B_MSH => Err(ImageValidationError::ProfileUnsupported {
            pc,
            code: instruction.code,
        }),
        _ => Ok(()),
    }
}

fn source_target(pc: usize, offset: usize, length: usize) -> Option<usize> {
    pc.checked_add(1)
        .and_then(|next| next.checked_add(offset))
        .filter(|target| *target < length)
}

fn validate_scratch_initialization(
    instructions: &[Instruction],
) -> Result<(), ImageValidationError> {
    let mut incoming = Vec::new();
    incoming
        .try_reserve_exact(instructions.len())
        .map_err(|_| ImageValidationError::NoMemory)?;
    incoming.resize(instructions.len(), None);
    incoming[0] = Some(0);

    for (pc, instruction) in instructions.iter().copied().enumerate() {
        let Some(mut initialized) = incoming[pc] else {
            continue;
        };
        match instruction.code {
            opcode::LD_MEM | opcode::LDX_MEM => {
                let bit = 1_u16 << instruction.k;
                if initialized & bit == 0 {
                    return Err(ImageValidationError::ScratchUninitialized {
                        pc,
                        index: instruction.k,
                    });
                }
            }
            opcode::ST | opcode::STX => initialized |= 1_u16 << instruction.k,
            _ => {}
        }

        match instruction.code {
            opcode::RET_K | opcode::RET_A => {}
            opcode::JMP_JA => {
                let Some(target) = source_target(pc, instruction.k as usize, instructions.len())
                else {
                    return Err(ImageValidationError::JumpOutOfRange { pc });
                };
                merge_initialized(&mut incoming[target], initialized);
            }
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X => {
                let Some(true_target) =
                    source_target(pc, usize::from(instruction.jt), instructions.len())
                else {
                    return Err(ImageValidationError::JumpOutOfRange { pc });
                };
                let Some(false_target) =
                    source_target(pc, usize::from(instruction.jf), instructions.len())
                else {
                    return Err(ImageValidationError::JumpOutOfRange { pc });
                };
                merge_initialized(&mut incoming[true_target], initialized);
                merge_initialized(&mut incoming[false_target], initialized);
            }
            _ => {
                let Some(next) = pc.checked_add(1) else {
                    return Err(ImageValidationError::JumpOutOfRange { pc });
                };
                if next < instructions.len() {
                    merge_initialized(&mut incoming[next], initialized);
                }
            }
        }
    }
    Ok(())
}

fn merge_initialized(slot: &mut Option<u16>, incoming: u16) {
    *slot = Some(slot.map(|old| old & incoming).unwrap_or(incoming));
}

const fn source_opcode_supported(code: u16) -> bool {
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

struct Checker<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Checker<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn position(&self) -> usize {
        self.position
    }

    fn validate_prologue(&mut self, profile: InputProfile) -> Result<(), ImageValidationError> {
        self.expect(0, NativeOp::Push(Register::Rbp))?;
        self.expect(0, NativeOp::Push(Register::R12))?;
        self.expect(0, NativeOp::Push(Register::R13))?;
        self.expect(0, NativeOp::Push(Register::R14))?;
        self.expect(0, NativeOp::Push(Register::R15))?;
        self.expect(
            0,
            mov(
                Width::Qword,
                register(Register::Rbp),
                register(Register::Rsp),
            ),
        )?;
        self.expect(
            0,
            NativeOp::Binary {
                kind: BinaryKind::Sub,
                width: Width::Qword,
                destination: register(Register::Rsp),
                source: immediate(0x40),
            },
        )?;
        match profile {
            InputProfile::PacketBytesBigEndian | InputProfile::NativeAlignedWords => {
                self.expect(
                    0,
                    mov(
                        Width::Qword,
                        register(Register::R12),
                        register(Register::Rdi),
                    ),
                )?;
                self.expect(
                    0,
                    mov(
                        Width::Word,
                        register(Register::R13d),
                        register(Register::Esi),
                    ),
                )?;
            }
            InputProfile::PacketContextBigEndian => {
                self.expect(
                    0,
                    NativeOp::Load {
                        width: Width::Qword,
                        destination: Register::R12,
                        base: Register::Rdi,
                        displacement: 0,
                    },
                )?;
                self.expect(
                    0,
                    NativeOp::Load {
                        width: Width::Word,
                        destination: Register::R13d,
                        base: Register::Rdi,
                        displacement: PacketInputContext::LEN_OFFSET as i8,
                    },
                )?;
            }
        }
        self.expect(
            0,
            NativeOp::Binary {
                kind: BinaryKind::Xor,
                width: Width::Word,
                destination: register(Register::Eax),
                source: register(Register::Eax),
            },
        )?;
        self.expect(
            0,
            NativeOp::Binary {
                kind: BinaryKind::Xor,
                width: Width::Word,
                destination: register(Register::Ecx),
                source: register(Register::Ecx),
            },
        )?;
        Ok(())
    }

    fn validate_epilogue(&mut self) -> Result<(), ImageValidationError> {
        self.expect(
            0,
            NativeOp::Binary {
                kind: BinaryKind::Add,
                width: Width::Qword,
                destination: register(Register::Rsp),
                source: immediate(0x40),
            },
        )?;
        self.expect(0, NativeOp::Pop(Register::R15))?;
        self.expect(0, NativeOp::Pop(Register::R14))?;
        self.expect(0, NativeOp::Pop(Register::R13))?;
        self.expect(0, NativeOp::Pop(Register::R12))?;
        self.expect(0, NativeOp::Pop(Register::Rbp))?;
        self.expect(0, NativeOp::Return)?;
        Ok(())
    }

    fn validate_source_instruction(
        &mut self,
        pc: usize,
        instruction: Instruction,
        profile: InputProfile,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        match instruction.code {
            opcode::LD_IMM => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Eax),
                    immediate(instruction.k),
                ),
            )?,
            opcode::LD_W_ABS => {
                self.validate_absolute_load(pc, instruction.k, LoadWidth::Word, profile, targets)?
            }
            opcode::LD_H_ABS => {
                self.validate_absolute_load(pc, instruction.k, LoadWidth::Half, profile, targets)?
            }
            opcode::LD_B_ABS => {
                self.validate_absolute_load(pc, instruction.k, LoadWidth::Byte, profile, targets)?
            }
            opcode::LD_W_IND => {
                self.validate_indirect_load(pc, instruction.k, LoadWidth::Word, profile, targets)?
            }
            opcode::LD_H_IND => {
                self.validate_indirect_load(pc, instruction.k, LoadWidth::Half, profile, targets)?
            }
            opcode::LD_B_IND => {
                self.validate_indirect_load(pc, instruction.k, LoadWidth::Byte, profile, targets)?
            }
            opcode::LD_MEM => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Eax),
                    scratch(instruction.k, Register::Rbp),
                ),
            )?,
            opcode::LD_LEN => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Eax),
                    register(Register::R13d),
                ),
            )?,
            opcode::LDX_IMM => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Ecx),
                    immediate(instruction.k),
                ),
            )?,
            opcode::LDX_MEM => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Ecx),
                    scratch(instruction.k, Register::Rbp),
                ),
            )?,
            opcode::LDX_LEN => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::R13d),
                ),
            )?,
            opcode::LDX_B_MSH => {
                self.validate_absolute_load_to_x(pc, instruction.k, profile, targets)?;
                self.expect(
                    pc,
                    NativeOp::Binary {
                        kind: BinaryKind::And,
                        width: Width::Word,
                        destination: register(Register::Ecx),
                        source: immediate(0x0f),
                    },
                )?;
                self.expect(
                    pc,
                    NativeOp::ShiftImmediate {
                        kind: ShiftKind::Left,
                        width: Width::Word,
                        destination: Register::Ecx,
                        count: 2,
                    },
                )?;
            }
            opcode::ST => self.expect(
                pc,
                mov(
                    Width::Word,
                    scratch(instruction.k, Register::Rbp),
                    register(Register::Eax),
                ),
            )?,
            opcode::STX => self.expect(
                pc,
                mov(
                    Width::Word,
                    scratch(instruction.k, Register::Rbp),
                    register(Register::Ecx),
                ),
            )?,
            opcode::ALU_ADD_K
            | opcode::ALU_SUB_K
            | opcode::ALU_OR_K
            | opcode::ALU_AND_K
            | opcode::ALU_XOR_K => self.expect(
                pc,
                NativeOp::Binary {
                    kind: binary_kind(instruction.code),
                    width: Width::Word,
                    destination: register(Register::Eax),
                    source: immediate(instruction.k),
                },
            )?,
            opcode::ALU_ADD_X
            | opcode::ALU_SUB_X
            | opcode::ALU_OR_X
            | opcode::ALU_AND_X
            | opcode::ALU_XOR_X => self.expect(
                pc,
                NativeOp::Binary {
                    kind: binary_kind(instruction.code),
                    width: Width::Word,
                    destination: register(Register::Eax),
                    source: register(Register::Ecx),
                },
            )?,
            opcode::ALU_MUL_K => self.expect(
                pc,
                NativeOp::ImmediateMultiply {
                    destination: Register::Eax,
                    source: Register::Eax,
                    immediate: instruction.k,
                },
            )?,
            opcode::ALU_MUL_X => self.expect(
                pc,
                NativeOp::Binary {
                    kind: BinaryKind::Mul,
                    width: Width::Word,
                    destination: register(Register::Eax),
                    source: register(Register::Ecx),
                },
            )?,
            opcode::ALU_DIV_K | opcode::ALU_MOD_K => {
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::R14d),
                        immediate(instruction.k),
                    ),
                )?;
                self.expect(
                    pc,
                    NativeOp::Binary {
                        kind: BinaryKind::Xor,
                        width: Width::Word,
                        destination: register(Register::Edx),
                        source: register(Register::Edx),
                    },
                )?;
                self.expect(
                    pc,
                    NativeOp::Divide {
                        width: Width::Word,
                        divisor: register(Register::R14d),
                    },
                )?;
                if instruction.code == opcode::ALU_MOD_K {
                    self.expect(
                        pc,
                        mov(
                            Width::Word,
                            register(Register::Eax),
                            register(Register::Edx),
                        ),
                    )?;
                }
            }
            opcode::ALU_DIV_X | opcode::ALU_MOD_X => {
                self.expect_test(
                    pc,
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::Ecx),
                )?;
                let target = self.expect_conditional_jump(pc, Condition::Equal)?;
                self.record_target(targets, pc, target, Target::Failure);
                self.expect(
                    pc,
                    NativeOp::Binary {
                        kind: BinaryKind::Xor,
                        width: Width::Word,
                        destination: register(Register::Edx),
                        source: register(Register::Edx),
                    },
                )?;
                self.expect(
                    pc,
                    NativeOp::Divide {
                        width: Width::Word,
                        divisor: register(Register::Ecx),
                    },
                )?;
                if instruction.code == opcode::ALU_MOD_X {
                    self.expect(
                        pc,
                        mov(
                            Width::Word,
                            register(Register::Eax),
                            register(Register::Edx),
                        ),
                    )?;
                }
            }
            opcode::ALU_LSH_K | opcode::ALU_RSH_K => self.expect(
                pc,
                NativeOp::ShiftImmediate {
                    kind: if instruction.code == opcode::ALU_LSH_K {
                        ShiftKind::Left
                    } else {
                        ShiftKind::Right
                    },
                    width: Width::Word,
                    destination: Register::Eax,
                    count: instruction.k as u8,
                },
            )?,
            opcode::ALU_LSH_X | opcode::ALU_RSH_X => {
                let kind = if instruction.code == opcode::ALU_LSH_X {
                    ShiftKind::Left
                } else {
                    ShiftKind::Right
                };
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::R14d),
                        register(Register::Ecx),
                    ),
                )?;
                self.expect(
                    pc,
                    NativeOp::Binary {
                        kind: BinaryKind::And,
                        width: Width::Word,
                        destination: register(Register::R14d),
                        source: immediate(0x1f),
                    },
                )?;
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::R15d),
                        register(Register::Ecx),
                    ),
                )?;
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::Ecx),
                        register(Register::R14d),
                    ),
                )?;
                self.expect(
                    pc,
                    NativeOp::ShiftRegister {
                        kind,
                        width: Width::Word,
                        destination: Register::Eax,
                    },
                )?;
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::Ecx),
                        register(Register::R15d),
                    ),
                )?;
            }
            opcode::ALU_NEG => self.expect(
                pc,
                NativeOp::Unary {
                    kind: UnaryKind::Neg,
                    width: Width::Word,
                    destination: Register::Eax,
                    immediate: 0,
                },
            )?,
            opcode::JMP_JA => {
                let target = self.expect_jump(pc)?;
                self.record_target(
                    targets,
                    pc,
                    target,
                    Target::Source(pc + 1 + instruction.k as usize),
                );
            }
            opcode::JMP_JEQ_K
            | opcode::JMP_JEQ_X
            | opcode::JMP_JGT_K
            | opcode::JMP_JGT_X
            | opcode::JMP_JGE_K
            | opcode::JMP_JGE_X
            | opcode::JMP_JSET_K
            | opcode::JMP_JSET_X => {
                let condition = jump_condition(instruction.code);
                let left = register(Register::Eax);
                let right = if instruction.code & 0x08 == 0 {
                    immediate(instruction.k)
                } else {
                    register(Register::Ecx)
                };
                if matches!(instruction.code, opcode::JMP_JSET_K | opcode::JMP_JSET_X) {
                    self.expect_test(pc, Width::Word, left, right)?
                } else {
                    self.expect_compare(pc, Width::Word, left, right)?
                }
                let target = self.expect_conditional_jump(pc, condition)?;
                self.record_target(
                    targets,
                    pc,
                    target,
                    Target::Source(pc + 1 + usize::from(instruction.jt)),
                );
                let target = self.expect_jump(pc)?;
                self.record_target(
                    targets,
                    pc,
                    target,
                    Target::Source(pc + 1 + usize::from(instruction.jf)),
                );
            }
            opcode::RET_K => {
                self.expect(
                    pc,
                    mov(
                        Width::Word,
                        register(Register::Eax),
                        immediate(instruction.k),
                    ),
                )?;
                let target = self.expect_jump(pc)?;
                self.record_target(targets, pc, target, Target::Epilogue);
            }
            opcode::RET_A => {
                let target = self.expect_jump(pc)?;
                self.record_target(targets, pc, target, Target::Epilogue);
            }
            opcode::MISC_TAX => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::Eax),
                ),
            )?,
            opcode::MISC_TXA => self.expect(
                pc,
                mov(
                    Width::Word,
                    register(Register::Eax),
                    register(Register::Ecx),
                ),
            )?,
            _ => {
                return Err(ImageValidationError::NativeSemanticMismatch {
                    pc,
                    offset: self.position,
                });
            }
        }
        Ok(())
    }

    fn validate_absolute_load(
        &mut self,
        pc: usize,
        offset: u32,
        width: LoadWidth,
        profile: InputProfile,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        if let Some(field) = ancillary_from_offset(offset) {
            if !matches!(profile, InputProfile::PacketContextBigEndian) {
                return Err(ImageValidationError::ProfileUnsupported {
                    pc,
                    code: match width {
                        LoadWidth::Word => opcode::LD_W_ABS,
                        LoadWidth::Half => opcode::LD_H_ABS,
                        LoadWidth::Byte => opcode::LD_B_ABS,
                    },
                });
            }
            let displacement =
                i8::try_from(PacketInputContext::METADATA_OFFSET + field.metadata_offset())
                    .map_err(|_| ImageValidationError::NativeSemanticMismatch {
                        pc,
                        offset: self.position,
                    })?;
            self.expect(
                pc,
                NativeOp::Load {
                    width: Width::Word,
                    destination: Register::Eax,
                    base: Register::Rdi,
                    displacement,
                },
            )?;
            return Ok(());
        }
        self.expect(
            pc,
            mov(Width::Word, register(Register::R14d), immediate(offset)),
        )?;
        self.validate_bounds(pc, width, false, targets)?;
        self.validate_address_add(pc, targets)?;
        self.expect(pc, NativeOp::SpeculationBarrier)?;
        self.expect(
            pc,
            NativeOp::Load {
                width: native_load_width(width),
                destination: Register::Eax,
                base: Register::R14,
                displacement: 0,
            },
        )?;
        self.validate_endian_transform(pc, width, profile)
    }

    fn validate_absolute_load_to_x(
        &mut self,
        pc: usize,
        offset: u32,
        _profile: InputProfile,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        if ancillary_from_offset(offset).is_some() {
            return Err(ImageValidationError::UnsupportedAncillaryLoad { pc, offset });
        }
        self.expect(
            pc,
            mov(Width::Word, register(Register::R14d), immediate(offset)),
        )?;
        self.validate_bounds(pc, LoadWidth::Byte, false, targets)?;
        self.validate_address_add(pc, targets)?;
        self.expect(pc, NativeOp::SpeculationBarrier)?;
        self.expect(
            pc,
            NativeOp::Load {
                width: Width::Byte,
                destination: Register::Ecx,
                base: Register::R14,
                displacement: 0,
            },
        )?;
        Ok(())
    }

    fn validate_indirect_load(
        &mut self,
        pc: usize,
        offset: u32,
        width: LoadWidth,
        profile: InputProfile,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        self.expect(
            pc,
            mov(
                Width::Word,
                register(Register::R14d),
                register(Register::Ecx),
            ),
        )?;
        self.expect(
            pc,
            NativeOp::Binary {
                kind: BinaryKind::Add,
                width: Width::Word,
                destination: register(Register::R14d),
                source: immediate(offset),
            },
        )?;
        let target = self.expect_conditional_jump(pc, Condition::Below)?;
        self.record_target(targets, pc, target, Target::Failure);
        self.validate_bounds(pc, width, true, targets)?;
        if matches!(profile, InputProfile::NativeAlignedWords) && matches!(width, LoadWidth::Word) {
            self.expect(
                pc,
                NativeOp::Test {
                    width: Width::Byte,
                    left: register(Register::R14d),
                    right: immediate(3),
                },
            )?;
            let target = self.expect_conditional_jump(pc, Condition::NotEqual)?;
            self.record_target(targets, pc, target, Target::Failure);
        }
        self.validate_address_add(pc, targets)?;
        self.expect(pc, NativeOp::SpeculationBarrier)?;
        self.expect(
            pc,
            NativeOp::Load {
                width: native_load_width(width),
                destination: Register::Eax,
                base: Register::R14,
                displacement: 0,
            },
        )?;
        self.validate_endian_transform(pc, width, profile)
    }

    fn validate_bounds(
        &mut self,
        pc: usize,
        width: LoadWidth,
        indirect: bool,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        let width = width_bytes(width) as u32;
        self.expect(
            pc,
            NativeOp::Compare {
                width: Width::Word,
                left: register(Register::R13d),
                right: immediate(width),
            },
        )?;
        let target = self.expect_conditional_jump(pc, Condition::Below)?;
        self.record_target(targets, pc, target, Target::Failure);
        self.expect(
            pc,
            mov(
                Width::Word,
                register(Register::R15d),
                register(Register::R13d),
            ),
        )?;
        self.expect(
            pc,
            NativeOp::Binary {
                kind: BinaryKind::Sub,
                width: Width::Word,
                destination: register(Register::R15d),
                source: immediate(width),
            },
        )?;
        self.expect(
            pc,
            NativeOp::Compare {
                width: Width::Word,
                left: register(Register::R15d),
                right: register(Register::R14d),
            },
        )?;
        let target = self.expect_conditional_jump(pc, Condition::Below)?;
        self.record_target(targets, pc, target, Target::Failure);
        if indirect {
            // The profile-specific alignment check is emitted by the caller
            // after these bounds operations; keeping this argument explicit
            // prevents an absolute-load validator from accepting it by
            // accident.
        }
        Ok(())
    }

    fn validate_address_add(
        &mut self,
        pc: usize,
        targets: &mut Vec<TargetCheck>,
    ) -> Result<(), ImageValidationError> {
        self.expect(
            pc,
            NativeOp::Binary {
                kind: BinaryKind::Add,
                width: Width::Qword,
                destination: register(Register::R14),
                source: register(Register::R12),
            },
        )?;
        let target = self.expect_conditional_jump(pc, Condition::Below)?;
        self.record_target(targets, pc, target, Target::Failure);
        Ok(())
    }

    fn validate_endian_transform(
        &mut self,
        pc: usize,
        width: LoadWidth,
        profile: InputProfile,
    ) -> Result<(), ImageValidationError> {
        if !matches!(
            profile,
            InputProfile::PacketBytesBigEndian | InputProfile::PacketContextBigEndian
        ) {
            return Ok(());
        }
        let operation = match width {
            LoadWidth::Byte => return Ok(()),
            LoadWidth::Half => NativeOp::Unary {
                kind: UnaryKind::RotateLeft,
                width: Width::Half,
                destination: Register::Eax,
                immediate: 8,
            },
            LoadWidth::Word => NativeOp::Unary {
                kind: UnaryKind::Bswap,
                width: Width::Word,
                destination: Register::Eax,
                immediate: 0,
            },
        };
        self.expect(pc, operation)
    }

    fn expect(&mut self, pc: usize, expected: NativeOp) -> Result<(), ImageValidationError> {
        let decoded = self.decode()?;
        if decoded.operation != expected {
            return Err(ImageValidationError::NativeSemanticMismatch {
                pc,
                offset: decoded.offset,
            });
        }
        Ok(())
    }

    fn expect_test(
        &mut self,
        pc: usize,
        width: Width,
        left: Operand,
        right: Operand,
    ) -> Result<(), ImageValidationError> {
        self.expect(pc, NativeOp::Test { width, left, right })
    }

    fn expect_compare(
        &mut self,
        pc: usize,
        width: Width,
        left: Operand,
        right: Operand,
    ) -> Result<(), ImageValidationError> {
        self.expect(pc, NativeOp::Compare { width, left, right })
    }

    fn expect_conditional_jump(
        &mut self,
        pc: usize,
        condition: Condition,
    ) -> Result<usize, ImageValidationError> {
        let decoded = self.decode()?;
        match decoded.operation {
            NativeOp::ConditionalJump {
                condition: actual,
                target,
            } if actual == condition => Ok(target),
            _ => Err(ImageValidationError::NativeSemanticMismatch {
                pc,
                offset: decoded.offset,
            }),
        }
    }

    fn expect_jump(&mut self, pc: usize) -> Result<usize, ImageValidationError> {
        let decoded = self.decode()?;
        match decoded.operation {
            NativeOp::Jump { target } => Ok(target),
            _ => Err(ImageValidationError::NativeSemanticMismatch {
                pc,
                offset: decoded.offset,
            }),
        }
    }

    fn record_target(
        &mut self,
        targets: &mut Vec<TargetCheck>,
        pc: usize,
        actual: usize,
        expected: Target,
    ) {
        targets.push(TargetCheck {
            pc,
            actual,
            expected,
        });
    }

    fn decode(&mut self) -> Result<Decoded, ImageValidationError> {
        let offset = self.position;
        let operation = self
            .decode_operation()
            .ok_or(ImageValidationError::NativeDecode { offset })?;
        Ok(Decoded { offset, operation })
    }

    fn decode_operation(&mut self) -> Option<NativeOp> {
        let first = self.byte()?;
        match first {
            0x55 => Some(NativeOp::Push(Register::Rbp)),
            0x5d => Some(NativeOp::Pop(Register::Rbp)),
            0x41 => self.decode_rex_b(),
            0x44 => self.decode_rex_r(),
            0x45 => self.decode_rex_rb(),
            0x48 => self.decode_rex_w(),
            0x49 => self.decode_rex_wb(),
            0x4c => self.decode_rex_wr(),
            0x4d => self.decode_rex_wrb(),
            0x0f => self.decode_0f(),
            0x31 => match self.byte()? {
                0xc0 => Some(binary_xor(Register::Eax, Register::Eax)),
                0xc8 => Some(binary_xor(Register::Eax, Register::Ecx)),
                0xc9 => Some(binary_xor(Register::Ecx, Register::Ecx)),
                0xd2 => Some(binary_xor(Register::Edx, Register::Edx)),
                _ => None,
            },
            0x01 => self.decode_binary_register(BinaryKind::Add),
            0x09 => self.decode_binary_register(BinaryKind::Or),
            0x21 => self.decode_binary_register(BinaryKind::And),
            0x29 => self.decode_binary_register(BinaryKind::Sub),
            0x39 => {
                if self.byte()? == 0xc8 {
                    Some(NativeOp::Compare {
                        width: Width::Word,
                        left: register(Register::Eax),
                        right: register(Register::Ecx),
                    })
                } else {
                    None
                }
            }
            0x05 => Some(binary_immediate(BinaryKind::Add, self.imm32()?)),
            0x0d => Some(binary_immediate(BinaryKind::Or, self.imm32()?)),
            0x25 => Some(binary_immediate(BinaryKind::And, self.imm32()?)),
            0x2d => Some(binary_immediate(BinaryKind::Sub, self.imm32()?)),
            0x35 => Some(binary_immediate(BinaryKind::Xor, self.imm32()?)),
            0x3d => Some(NativeOp::Compare {
                width: Width::Word,
                left: register(Register::Eax),
                right: immediate(self.imm32()?),
            }),
            0x66 => {
                if self.byte()? == 0xc1 && self.byte()? == 0xc0 && self.byte()? == 8 {
                    Some(NativeOp::Unary {
                        kind: UnaryKind::RotateLeft,
                        width: Width::Half,
                        destination: Register::Eax,
                        immediate: 8,
                    })
                } else {
                    None
                }
            }
            0x69 => {
                if self.byte()? == 0xc0 {
                    Some(NativeOp::ImmediateMultiply {
                        destination: Register::Eax,
                        source: Register::Eax,
                        immediate: self.imm32()?,
                    })
                } else {
                    None
                }
            }
            0x83 => match (self.byte()?, self.byte()?) {
                (0xe0, 0x0f) => Some(NativeOp::Binary {
                    kind: BinaryKind::And,
                    width: Width::Word,
                    destination: register(Register::Eax),
                    source: immediate(0x0f),
                }),
                (0xe1, 0x0f) => Some(NativeOp::Binary {
                    kind: BinaryKind::And,
                    width: Width::Word,
                    destination: register(Register::Ecx),
                    source: immediate(0x0f),
                }),
                _ => None,
            },
            0x85 => match self.byte()? {
                0xc8 => Some(NativeOp::Test {
                    width: Width::Word,
                    left: register(Register::Eax),
                    right: register(Register::Ecx),
                }),
                0xc9 => Some(NativeOp::Test {
                    width: Width::Word,
                    left: register(Register::Ecx),
                    right: register(Register::Ecx),
                }),
                _ => None,
            },
            0x89 => self.decode_move_register_or_scratch(),
            0x8b => match self.byte()? {
                0x06 => Some(NativeOp::Load {
                    width: Width::Word,
                    destination: Register::Eax,
                    base: Register::R14,
                    displacement: 0,
                }),
                0x47 => Some(NativeOp::Load {
                    width: Width::Word,
                    destination: Register::Eax,
                    base: Register::Rdi,
                    displacement: self.byte()? as i8,
                }),
                0x45 => Some(mov(
                    Width::Word,
                    register(Register::Eax),
                    memory(Register::Rbp, self.byte()? as i8),
                )),
                0x4d => Some(mov(
                    Width::Word,
                    register(Register::Ecx),
                    memory(Register::Rbp, self.byte()? as i8),
                )),
                _ => None,
            },
            0xa9 => Some(NativeOp::Test {
                width: Width::Word,
                left: register(Register::Eax),
                right: immediate(self.imm32()?),
            }),
            0xb8 => Some(mov(
                Width::Word,
                register(Register::Eax),
                immediate(self.imm32()?),
            )),
            0xb9 => Some(mov(
                Width::Word,
                register(Register::Ecx),
                immediate(self.imm32()?),
            )),
            0xc1 => match (self.byte()?, self.byte()?) {
                (0xe0, count) => Some(NativeOp::ShiftImmediate {
                    kind: ShiftKind::Left,
                    width: Width::Word,
                    destination: Register::Eax,
                    count,
                }),
                (0xe1, count) => Some(NativeOp::ShiftImmediate {
                    kind: ShiftKind::Left,
                    width: Width::Word,
                    destination: Register::Ecx,
                    count,
                }),
                (0xe8, count) => Some(NativeOp::ShiftImmediate {
                    kind: ShiftKind::Right,
                    width: Width::Word,
                    destination: Register::Eax,
                    count,
                }),
                _ => None,
            },
            0xd3 => match self.byte()? {
                0xe0 => Some(NativeOp::ShiftRegister {
                    kind: ShiftKind::Left,
                    width: Width::Word,
                    destination: Register::Eax,
                }),
                0xe8 => Some(NativeOp::ShiftRegister {
                    kind: ShiftKind::Right,
                    width: Width::Word,
                    destination: Register::Eax,
                }),
                _ => None,
            },
            0xe9 => Some(NativeOp::Jump {
                target: self.relative_target()?,
            }),
            0xf7 => match self.byte()? {
                0xd8 => Some(NativeOp::Unary {
                    kind: UnaryKind::Neg,
                    width: Width::Word,
                    destination: Register::Eax,
                    immediate: 0,
                }),
                0xf1 => Some(NativeOp::Divide {
                    width: Width::Word,
                    divisor: register(Register::Ecx),
                }),
                _ => None,
            },
            0xc3 => Some(NativeOp::Return),
            _ => None,
        }
    }

    fn decode_rex_b(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0x54..=0x57 => Some(NativeOp::Push(match self.bytes[self.position - 1] {
                0x54 => Register::R12,
                0x55 => Register::R13,
                0x56 => Register::R14,
                _ => Register::R15,
            })),
            0x5c..=0x5f => Some(NativeOp::Pop(match self.bytes[self.position - 1] {
                0x5c => Register::R12,
                0x5d => Register::R13,
                0x5e => Register::R14,
                _ => Register::R15,
            })),
            0x81 => {
                if self.byte()? == 0xc6 {
                    Some(NativeOp::Binary {
                        kind: BinaryKind::Add,
                        width: Width::Word,
                        destination: register(Register::R14d),
                        source: immediate(self.imm32()?),
                    })
                } else {
                    None
                }
            }
            0x83 => match self.byte()? {
                0xe6 => Some(NativeOp::Binary {
                    kind: BinaryKind::And,
                    width: Width::Word,
                    destination: register(Register::R14d),
                    source: immediate(self.byte()? as u32),
                }),
                0xef => Some(NativeOp::Binary {
                    kind: BinaryKind::Sub,
                    width: Width::Word,
                    destination: register(Register::R15d),
                    source: immediate(self.byte()? as u32),
                }),
                0xfd => Some(NativeOp::Compare {
                    width: Width::Word,
                    left: register(Register::R13d),
                    right: immediate(self.byte()? as u32),
                }),
                _ => None,
            },
            0x89 => match self.byte()? {
                0xce => Some(mov(
                    Width::Word,
                    register(Register::R14d),
                    register(Register::Ecx),
                )),
                0xcf => Some(mov(
                    Width::Word,
                    register(Register::R15d),
                    register(Register::Ecx),
                )),
                0xf5 => Some(mov(
                    Width::Word,
                    register(Register::R13d),
                    register(Register::Esi),
                )),
                _ => None,
            },
            0x8b => {
                if self.byte()? == 0x06 {
                    Some(NativeOp::Load {
                        width: Width::Word,
                        destination: Register::Eax,
                        base: Register::R14,
                        displacement: 0,
                    })
                } else {
                    None
                }
            }
            0x0f => match self.byte()? {
                0xb6 => match self.byte()? {
                    0x06 => Some(NativeOp::Load {
                        width: Width::Byte,
                        destination: Register::Eax,
                        base: Register::R14,
                        displacement: 0,
                    }),
                    0x0e => Some(NativeOp::Load {
                        width: Width::Byte,
                        destination: Register::Ecx,
                        base: Register::R14,
                        displacement: 0,
                    }),
                    _ => None,
                },
                0xb7 => {
                    if self.byte()? == 0x06 {
                        Some(NativeOp::Load {
                            width: Width::Half,
                            destination: Register::Eax,
                            base: Register::R14,
                            displacement: 0,
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            },
            0xf6 => {
                if self.byte()? == 0xc6 && self.byte()? == 3 {
                    Some(NativeOp::Test {
                        width: Width::Byte,
                        left: register(Register::R14d),
                        right: immediate(3),
                    })
                } else {
                    None
                }
            }
            0xf7 => {
                if self.byte()? == 0xf6 {
                    Some(NativeOp::Divide {
                        width: Width::Word,
                        divisor: register(Register::R14d),
                    })
                } else {
                    None
                }
            }
            0xbe => Some(mov(
                Width::Word,
                register(Register::R14d),
                immediate(self.imm32()?),
            )),
            _ => None,
        }
    }

    fn decode_rex_r(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0x8b => {
                if self.byte()? == 0x6f {
                    Some(NativeOp::Load {
                        width: Width::Word,
                        destination: Register::R13d,
                        base: Register::Rdi,
                        displacement: self.byte()? as i8,
                    })
                } else {
                    None
                }
            }
            0x89 => match self.byte()? {
                0xe8 => Some(mov(
                    Width::Word,
                    register(Register::Eax),
                    register(Register::R13d),
                )),
                0xe9 => Some(mov(
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::R13d),
                )),
                0xf1 => Some(mov(
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::R14d),
                )),
                0xf9 => Some(mov(
                    Width::Word,
                    register(Register::Ecx),
                    register(Register::R15d),
                )),
                _ => None,
            },
            _ => None,
        }
    }

    fn decode_rex_wr(&mut self) -> Option<NativeOp> {
        if self.byte()? == 0x8b && self.byte()? == 0x27 {
            Some(NativeOp::Load {
                width: Width::Qword,
                destination: Register::R12,
                base: Register::Rdi,
                displacement: 0,
            })
        } else {
            None
        }
    }

    fn decode_rex_rb(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0x89 if self.byte()? == 0xef => Some(mov(
                Width::Word,
                register(Register::R15d),
                register(Register::R13d),
            )),
            0x39 if self.byte()? == 0xf7 => Some(NativeOp::Compare {
                width: Width::Word,
                left: register(Register::R15d),
                right: register(Register::R14d),
            }),
            _ => None,
        }
    }

    fn decode_rex_w(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0x83 => match self.byte()? {
                0xec => Some(NativeOp::Binary {
                    kind: BinaryKind::Sub,
                    width: Width::Qword,
                    destination: register(Register::Rsp),
                    source: immediate(self.byte()? as u32),
                }),
                0xc4 => Some(NativeOp::Binary {
                    kind: BinaryKind::Add,
                    width: Width::Qword,
                    destination: register(Register::Rsp),
                    source: immediate(self.byte()? as u32),
                }),
                _ => None,
            },
            0x89 => {
                if self.byte()? == 0xe5 {
                    Some(mov(
                        Width::Qword,
                        register(Register::Rbp),
                        register(Register::Rsp),
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn decode_rex_wb(&mut self) -> Option<NativeOp> {
        if self.byte()? == 0x89 && self.byte()? == 0xfc {
            Some(mov(
                Width::Qword,
                register(Register::R12),
                register(Register::Rdi),
            ))
        } else {
            None
        }
    }

    fn decode_rex_wrb(&mut self) -> Option<NativeOp> {
        if self.byte()? == 0x01 && self.byte()? == 0xe6 {
            Some(NativeOp::Binary {
                kind: BinaryKind::Add,
                width: Width::Qword,
                destination: register(Register::R14),
                source: register(Register::R12),
            })
        } else {
            None
        }
    }

    fn decode_0f(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0xae => {
                if self.byte()? == 0xe8 {
                    Some(NativeOp::SpeculationBarrier)
                } else {
                    None
                }
            }
            0x82 => Some(NativeOp::ConditionalJump {
                condition: Condition::Below,
                target: self.relative_target()?,
            }),
            0x83 => Some(NativeOp::ConditionalJump {
                condition: Condition::AboveOrEqual,
                target: self.relative_target()?,
            }),
            0x84 => Some(NativeOp::ConditionalJump {
                condition: Condition::Equal,
                target: self.relative_target()?,
            }),
            0x85 => Some(NativeOp::ConditionalJump {
                condition: Condition::NotEqual,
                target: self.relative_target()?,
            }),
            0x87 => Some(NativeOp::ConditionalJump {
                condition: Condition::Above,
                target: self.relative_target()?,
            }),
            0xaf => {
                if self.byte()? == 0xc1 {
                    Some(NativeOp::Binary {
                        kind: BinaryKind::Mul,
                        width: Width::Word,
                        destination: register(Register::Eax),
                        source: register(Register::Ecx),
                    })
                } else {
                    None
                }
            }
            0xc8 => Some(NativeOp::Unary {
                kind: UnaryKind::Bswap,
                width: Width::Word,
                destination: Register::Eax,
                immediate: 0,
            }),
            _ => None,
        }
    }

    fn decode_binary_register(&mut self, kind: BinaryKind) -> Option<NativeOp> {
        if self.byte()? == 0xc8 {
            Some(NativeOp::Binary {
                kind,
                width: Width::Word,
                destination: register(Register::Eax),
                source: register(Register::Ecx),
            })
        } else {
            None
        }
    }

    fn decode_move_register_or_scratch(&mut self) -> Option<NativeOp> {
        match self.byte()? {
            0xc1 => Some(mov(
                Width::Word,
                register(Register::Ecx),
                register(Register::Eax),
            )),
            0xc8 => Some(mov(
                Width::Word,
                register(Register::Eax),
                register(Register::Ecx),
            )),
            0xd0 => Some(mov(
                Width::Word,
                register(Register::Eax),
                register(Register::Edx),
            )),
            0x45 => Some(mov(
                Width::Word,
                memory(Register::Rbp, self.byte()? as i8),
                register(Register::Eax),
            )),
            0x4d => Some(mov(
                Width::Word,
                memory(Register::Rbp, self.byte()? as i8),
                register(Register::Ecx),
            )),
            _ => None,
        }
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn imm32(&mut self) -> Option<u32> {
        let bytes = self
            .bytes
            .get(self.position..self.position.checked_add(4)?)?;
        self.position += 4;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn relative_target(&mut self) -> Option<usize> {
        let displacement = i32::from_le_bytes(self.imm32()?.to_le_bytes());
        if displacement >= 0 {
            self.position.checked_add(displacement as usize)
        } else {
            self.position
                .checked_sub(displacement.unsigned_abs() as usize)
        }
    }
}

fn register(register: Register) -> Operand {
    Operand::Register(register)
}

fn immediate(value: u32) -> Operand {
    Operand::Immediate(value)
}

fn memory(base: Register, displacement: i8) -> Operand {
    Operand::Memory { base, displacement }
}

fn scratch(index: u32, base: Register) -> Operand {
    memory(base, scratch_displacement(index))
}

fn mov(width: Width, destination: Operand, source: Operand) -> NativeOp {
    NativeOp::Mov {
        width,
        destination,
        source,
    }
}

fn binary_xor(destination: Register, source: Register) -> NativeOp {
    NativeOp::Binary {
        kind: BinaryKind::Xor,
        width: Width::Word,
        destination: register(destination),
        source: register(source),
    }
}

fn binary_immediate(kind: BinaryKind, value: u32) -> NativeOp {
    NativeOp::Binary {
        kind,
        width: Width::Word,
        destination: register(Register::Eax),
        source: immediate(value),
    }
}

fn binary_kind(code: u16) -> BinaryKind {
    match code {
        opcode::ALU_ADD_K | opcode::ALU_ADD_X => BinaryKind::Add,
        opcode::ALU_SUB_K | opcode::ALU_SUB_X => BinaryKind::Sub,
        opcode::ALU_OR_K | opcode::ALU_OR_X => BinaryKind::Or,
        opcode::ALU_AND_K | opcode::ALU_AND_X => BinaryKind::And,
        opcode::ALU_XOR_K | opcode::ALU_XOR_X => BinaryKind::Xor,
        _ => BinaryKind::Xor,
    }
}

fn jump_condition(code: u16) -> Condition {
    match code {
        opcode::JMP_JEQ_K | opcode::JMP_JEQ_X => Condition::Equal,
        opcode::JMP_JGT_K | opcode::JMP_JGT_X => Condition::Above,
        opcode::JMP_JGE_K | opcode::JMP_JGE_X => Condition::AboveOrEqual,
        opcode::JMP_JSET_K | opcode::JMP_JSET_X => Condition::NotEqual,
        _ => Condition::Equal,
    }
}

fn native_load_width(width: LoadWidth) -> Width {
    match width {
        LoadWidth::Byte => Width::Byte,
        LoadWidth::Half => Width::Half,
        LoadWidth::Word => Width::Word,
    }
}

fn width_bytes(width: LoadWidth) -> usize {
    match width {
        LoadWidth::Byte => 1,
        LoadWidth::Half => 2,
        LoadWidth::Word => 4,
    }
}

fn scratch_displacement(index: u32) -> i8 {
    (-(4_i32 * (index as i32 + 1))) as i8
}
