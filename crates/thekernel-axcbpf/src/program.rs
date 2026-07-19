use alloc::vec::Vec;
use core::fmt;

use crate::{Instruction, opcode};

/// Maximum number of instructions in one verified classic-BPF program.
pub const MAX_INSTRUCTIONS: usize = 4096;

/// Number of 32-bit scratch words exposed by classic BPF.
pub const SCRATCH_WORDS: usize = 16;

/// Width requested from a classic-BPF input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadWidth {
    /// One byte.
    Byte,
    /// Two bytes.
    Half,
    /// Four bytes.
    Word,
}

impl LoadWidth {
    const fn bytes(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Half => 2,
            Self::Word => 4,
        }
    }
}

/// Read-only data source evaluated by a classic-BPF program.
///
/// The input owns load endianness and any domain-specific offset validation.
/// Returning `None` terminates evaluation with the classic-BPF failure value
/// zero. The interpreter never exposes A, X, scratch memory, or mutable state
/// to the input implementation.
pub trait Input {
    /// Returns the length value observed by `LD_LEN` and `LDX_LEN`.
    fn len(&self) -> u32;

    /// Returns whether the input length is zero.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Loads one value at an already resolved absolute offset.
    fn load(&self, offset: u32, width: LoadWidth) -> Option<u32>;
}

impl Input for [u8] {
    fn len(&self) -> u32 {
        u32::try_from(<[u8]>::len(self)).unwrap_or(u32::MAX)
    }

    fn load(&self, offset: u32, width: LoadWidth) -> Option<u32> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(width.bytes())?;
        let bytes = self.get(start..end)?;
        match width {
            LoadWidth::Byte => Some(u32::from(bytes[0])),
            LoadWidth::Half => Some(u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))),
            LoadWidth::Word => Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        }
    }
}

/// Failure while validating and owning one classic-BPF program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VerifyError {
    /// A program must contain at least one instruction.
    Empty,
    /// The program exceeds [`MAX_INSTRUCTIONS`].
    TooLong {
        /// Supplied instruction count.
        length: usize,
    },
    /// Storage needed for validation or the owned program could not be reserved.
    NoMemory,
    /// The opcode is not part of the supported ordinary classic-BPF set.
    UnsupportedOpcode {
        /// Instruction index.
        pc: usize,
        /// Rejected raw opcode.
        code: u16,
    },
    /// A negative encoded load offset selects an unsupported ancillary extension.
    UnsupportedAncillaryLoad {
        /// Instruction index.
        pc: usize,
        /// Rejected raw offset.
        offset: u32,
    },
    /// An immediate divisor or modulus was zero.
    ImmediateDivisionByZero {
        /// Instruction index.
        pc: usize,
    },
    /// An immediate shift count was outside `0..32`.
    ImmediateShiftOutOfRange {
        /// Instruction index.
        pc: usize,
        /// Rejected shift count.
        shift: u32,
    },
    /// A scratch-memory index was outside the sixteen-word store.
    ScratchOutOfRange {
        /// Instruction index.
        pc: usize,
        /// Rejected scratch index.
        index: u32,
    },
    /// A reachable path loads a scratch word not initialized on every path.
    ScratchUninitialized {
        /// Instruction index.
        pc: usize,
        /// Uninitialized scratch index.
        index: u32,
    },
    /// A jump target was outside the program.
    JumpOutOfRange {
        /// Instruction index.
        pc: usize,
    },
    /// The final instruction was not a return.
    MissingFinalReturn,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("classic-BPF program is empty"),
            Self::TooLong { length } => write!(
                formatter,
                "classic-BPF program has {length} instructions, limit is {MAX_INSTRUCTIONS}"
            ),
            Self::NoMemory => formatter.write_str("classic-BPF allocation failed"),
            Self::UnsupportedOpcode { pc, code } => {
                write!(
                    formatter,
                    "unsupported classic-BPF opcode {code:#x} at {pc}"
                )
            }
            Self::UnsupportedAncillaryLoad { pc, offset } => write!(
                formatter,
                "unsupported classic-BPF ancillary offset {offset:#x} at {pc}"
            ),
            Self::ImmediateDivisionByZero { pc } => {
                write!(formatter, "zero classic-BPF immediate divisor at {pc}")
            }
            Self::ImmediateShiftOutOfRange { pc, shift } => write!(
                formatter,
                "classic-BPF immediate shift {shift} is out of range at {pc}"
            ),
            Self::ScratchOutOfRange { pc, index } => write!(
                formatter,
                "classic-BPF scratch index {index} is out of range at {pc}"
            ),
            Self::ScratchUninitialized { pc, index } => write!(
                formatter,
                "classic-BPF scratch index {index} is not initialized on every path at {pc}"
            ),
            Self::JumpOutOfRange { pc } => {
                write!(formatter, "classic-BPF jump leaves the program at {pc}")
            }
            Self::MissingFinalReturn => {
                formatter.write_str("classic-BPF program does not end in a return")
            }
        }
    }
}

/// Immutable verified classic-BPF program.
///
/// Construction performs all structural checks and fallibly copies the raw
/// instructions. Evaluation allocates nothing, initializes A and X to zero,
/// and executes at most `len()` instructions because every accepted jump is
/// forward-only.
#[derive(Debug)]
pub struct Program {
    instructions: Vec<Instruction>,
}

impl Program {
    /// Verifies and fallibly takes an immutable copy of raw instructions.
    pub fn verify(instructions: &[Instruction]) -> Result<Self, VerifyError> {
        verify_program(instructions)?;

        let mut owned = Vec::new();
        owned
            .try_reserve_exact(instructions.len())
            .map_err(|_| VerifyError::NoMemory)?;
        owned.extend_from_slice(instructions);
        Ok(Self {
            instructions: owned,
        })
    }

    /// Verifies and takes ownership of an existing instruction vector.
    ///
    /// This path performs no second instruction-buffer allocation or copy. It
    /// is intended for adapters that already copied an untrusted wire program
    /// into a fallibly allocated kernel vector. Validation still uses bounded,
    /// fallible temporary storage for all-path scratch analysis.
    pub fn try_from_vec(instructions: Vec<Instruction>) -> Result<Self, VerifyError> {
        verify_program(&instructions)?;
        Ok(Self { instructions })
    }

    /// Returns the verified raw instructions.
    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }

    /// Returns the instruction count and maximum evaluation step count.
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Returns whether the program contains no instructions.
    ///
    /// Verified programs are never empty, so this always returns `false`.
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Evaluates the program without allocation.
    ///
    /// A failed input load, an indirect-offset overflow, or a run-time X
    /// divisor of zero terminates evaluation with zero, matching classic-BPF
    /// failure behavior. A and X start at zero for every call.
    pub fn evaluate<I: Input + ?Sized>(&self, input: &I) -> u32 {
        self.evaluate_counted(input).0
    }

    fn evaluate_counted<I: Input + ?Sized>(&self, input: &I) -> (u32, usize) {
        let mut accumulator = 0_u32;
        let mut index = 0_u32;
        let mut scratch = [0_u32; SCRATCH_WORDS];
        let mut pc = 0_usize;
        let mut steps = 0_usize;

        while steps < self.instructions.len() {
            let instruction = self.instructions[pc];
            steps += 1;

            match instruction.code {
                opcode::LD_IMM => accumulator = instruction.k,
                opcode::LD_W_ABS => {
                    let Some(value) = input.load(instruction.k, LoadWidth::Word) else {
                        return (0, steps);
                    };
                    accumulator = value;
                }
                opcode::LD_H_ABS => {
                    let Some(value) = input.load(instruction.k, LoadWidth::Half) else {
                        return (0, steps);
                    };
                    accumulator = value;
                }
                opcode::LD_B_ABS => {
                    let Some(value) = input.load(instruction.k, LoadWidth::Byte) else {
                        return (0, steps);
                    };
                    accumulator = value;
                }
                opcode::LD_W_IND | opcode::LD_H_IND | opcode::LD_B_IND => {
                    let Some(offset) = index.checked_add(instruction.k) else {
                        return (0, steps);
                    };
                    let width = match instruction.code {
                        opcode::LD_W_IND => LoadWidth::Word,
                        opcode::LD_H_IND => LoadWidth::Half,
                        opcode::LD_B_IND => LoadWidth::Byte,
                        _ => unreachable!(),
                    };
                    let Some(value) = input.load(offset, width) else {
                        return (0, steps);
                    };
                    accumulator = value;
                }
                opcode::LD_MEM => accumulator = scratch[instruction.k as usize],
                opcode::LD_LEN => accumulator = input.len(),
                opcode::LDX_IMM => index = instruction.k,
                opcode::LDX_MEM => index = scratch[instruction.k as usize],
                opcode::LDX_LEN => index = input.len(),
                opcode::LDX_B_MSH => {
                    let Some(value) = input.load(instruction.k, LoadWidth::Byte) else {
                        return (0, steps);
                    };
                    index = (value & 0x0f) << 2;
                }
                opcode::ST => scratch[instruction.k as usize] = accumulator,
                opcode::STX => scratch[instruction.k as usize] = index,
                opcode::ALU_ADD_K => accumulator = accumulator.wrapping_add(instruction.k),
                opcode::ALU_ADD_X => accumulator = accumulator.wrapping_add(index),
                opcode::ALU_SUB_K => accumulator = accumulator.wrapping_sub(instruction.k),
                opcode::ALU_SUB_X => accumulator = accumulator.wrapping_sub(index),
                opcode::ALU_MUL_K => accumulator = accumulator.wrapping_mul(instruction.k),
                opcode::ALU_MUL_X => accumulator = accumulator.wrapping_mul(index),
                opcode::ALU_DIV_K => accumulator /= instruction.k,
                opcode::ALU_DIV_X => {
                    if index == 0 {
                        return (0, steps);
                    }
                    accumulator /= index;
                }
                opcode::ALU_OR_K => accumulator |= instruction.k,
                opcode::ALU_OR_X => accumulator |= index,
                opcode::ALU_AND_K => accumulator &= instruction.k,
                opcode::ALU_AND_X => accumulator &= index,
                opcode::ALU_LSH_K => accumulator = accumulator.wrapping_shl(instruction.k),
                opcode::ALU_LSH_X => accumulator = accumulator.wrapping_shl(index & 31),
                opcode::ALU_RSH_K => accumulator = accumulator.wrapping_shr(instruction.k),
                opcode::ALU_RSH_X => accumulator = accumulator.wrapping_shr(index & 31),
                opcode::ALU_NEG => accumulator = accumulator.wrapping_neg(),
                opcode::ALU_MOD_K => accumulator %= instruction.k,
                opcode::ALU_MOD_X => {
                    if index == 0 {
                        return (0, steps);
                    }
                    accumulator %= index;
                }
                opcode::ALU_XOR_K => accumulator ^= instruction.k,
                opcode::ALU_XOR_X => accumulator ^= index,
                opcode::JMP_JA => {
                    pc += 1 + instruction.k as usize;
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
                    let operand = if matches!(
                        instruction.code,
                        opcode::JMP_JEQ_X
                            | opcode::JMP_JGT_X
                            | opcode::JMP_JGE_X
                            | opcode::JMP_JSET_X
                    ) {
                        index
                    } else {
                        instruction.k
                    };
                    let condition = match instruction.code {
                        opcode::JMP_JEQ_K | opcode::JMP_JEQ_X => accumulator == operand,
                        opcode::JMP_JGT_K | opcode::JMP_JGT_X => accumulator > operand,
                        opcode::JMP_JGE_K | opcode::JMP_JGE_X => accumulator >= operand,
                        opcode::JMP_JSET_K | opcode::JMP_JSET_X => accumulator & operand != 0,
                        _ => unreachable!(),
                    };
                    pc += 1 + usize::from(if condition {
                        instruction.jt
                    } else {
                        instruction.jf
                    });
                    continue;
                }
                opcode::RET_K => return (instruction.k, steps),
                opcode::RET_A => return (accumulator, steps),
                opcode::MISC_TAX => index = accumulator,
                opcode::MISC_TXA => accumulator = index,
                _ => unreachable!("verified program contains an unsupported opcode"),
            }

            pc += 1;
        }

        unreachable!("verified program exhausted its step bound without returning")
    }
}

fn verify_program(instructions: &[Instruction]) -> Result<(), VerifyError> {
    verify_structure(instructions)?;
    verify_scratch_initialization(instructions)
}

fn verify_structure(instructions: &[Instruction]) -> Result<(), VerifyError> {
    if instructions.is_empty() {
        return Err(VerifyError::Empty);
    }
    if instructions.len() > MAX_INSTRUCTIONS {
        return Err(VerifyError::TooLong {
            length: instructions.len(),
        });
    }

    for (pc, instruction) in instructions.iter().copied().enumerate() {
        if !is_supported(instruction.code) {
            return Err(VerifyError::UnsupportedOpcode {
                pc,
                code: instruction.code,
            });
        }

        match instruction.code {
            opcode::ALU_DIV_K | opcode::ALU_MOD_K if instruction.k == 0 => {
                return Err(VerifyError::ImmediateDivisionByZero { pc });
            }
            opcode::ALU_LSH_K | opcode::ALU_RSH_K if instruction.k >= 32 => {
                return Err(VerifyError::ImmediateShiftOutOfRange {
                    pc,
                    shift: instruction.k,
                });
            }
            opcode::LD_MEM | opcode::LDX_MEM | opcode::ST | opcode::STX
                if instruction.k >= SCRATCH_WORDS as u32 =>
            {
                return Err(VerifyError::ScratchOutOfRange {
                    pc,
                    index: instruction.k,
                });
            }
            opcode::JMP_JA => {
                let Some(target) = pc
                    .checked_add(1)
                    .and_then(|next| next.checked_add(instruction.k as usize))
                else {
                    return Err(VerifyError::JumpOutOfRange { pc });
                };
                if target >= instructions.len() {
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
                let true_target = pc + 1 + usize::from(instruction.jt);
                let false_target = pc + 1 + usize::from(instruction.jf);
                if true_target >= instructions.len() || false_target >= instructions.len() {
                    return Err(VerifyError::JumpOutOfRange { pc });
                }
            }
            opcode::LD_W_ABS | opcode::LD_H_ABS | opcode::LD_B_ABS | opcode::LDX_B_MSH
                if (instruction.k as i32) < 0 =>
            {
                return Err(VerifyError::UnsupportedAncillaryLoad {
                    pc,
                    offset: instruction.k,
                });
            }
            _ => {}
        }
    }

    if !matches!(
        instructions.last().map(|instruction| instruction.code),
        Some(opcode::RET_K | opcode::RET_A)
    ) {
        return Err(VerifyError::MissingFinalReturn);
    }

    Ok(())
}

fn verify_scratch_initialization(instructions: &[Instruction]) -> Result<(), VerifyError> {
    let mut incoming = Vec::new();
    incoming
        .try_reserve_exact(instructions.len())
        .map_err(|_| VerifyError::NoMemory)?;
    incoming.resize(instructions.len(), None);
    incoming[0] = Some(0_u16);

    for (pc, instruction) in instructions.iter().copied().enumerate() {
        let Some(mut initialized) = incoming[pc] else {
            continue;
        };

        match instruction.code {
            opcode::LD_MEM | opcode::LDX_MEM => {
                let bit = 1_u16 << instruction.k;
                if initialized & bit == 0 {
                    return Err(VerifyError::ScratchUninitialized {
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
                let target = pc + 1 + instruction.k as usize;
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
                let true_target = pc + 1 + usize::from(instruction.jt);
                let false_target = pc + 1 + usize::from(instruction.jf);
                merge_initialized(&mut incoming[true_target], initialized);
                merge_initialized(&mut incoming[false_target], initialized);
            }
            _ => merge_initialized(&mut incoming[pc + 1], initialized),
        }
    }

    Ok(())
}

fn merge_initialized(slot: &mut Option<u16>, incoming: u16) {
    *slot = Some(match *slot {
        Some(existing) => existing & incoming,
        None => incoming,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_execution_never_exceeds_program_length() {
        let program = Program::verify(&[
            Instruction::statement(opcode::LD_IMM, 1),
            Instruction::jump(opcode::JMP_JEQ_K, 1, 1, 0),
            Instruction::statement(opcode::RET_K, 0),
            Instruction::statement(opcode::RET_K, 1),
        ])
        .unwrap();
        let (_, steps) = program.evaluate_counted(&[][..]);
        assert!(steps <= program.len());
        assert_eq!(steps, 3);
    }
}
