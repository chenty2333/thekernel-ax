/// Raw classic-BPF opcode values.
///
/// These values match the long-standing `sock_filter.code` encoding. They are
/// mechanism identifiers, not Linux syscall or seccomp policy constants.
pub mod opcode {
    /// Load an immediate into A.
    pub const LD_IMM: u16 = 0x00;
    /// Load an input-defined 32-bit word at an absolute offset into A.
    pub const LD_W_ABS: u16 = 0x20;
    /// Load an input-defined 16-bit halfword at an absolute offset into A.
    pub const LD_H_ABS: u16 = 0x28;
    /// Load an input byte at an absolute offset into A.
    pub const LD_B_ABS: u16 = 0x30;
    /// Load a 32-bit input word at `X + k` into A.
    pub const LD_W_IND: u16 = 0x40;
    /// Load a 16-bit input halfword at `X + k` into A.
    pub const LD_H_IND: u16 = 0x48;
    /// Load an input byte at `X + k` into A.
    pub const LD_B_IND: u16 = 0x50;
    /// Load scratch word `M[k]` into A.
    pub const LD_MEM: u16 = 0x60;
    /// Load the input length into A.
    pub const LD_LEN: u16 = 0x80;

    /// Load an immediate into X.
    pub const LDX_IMM: u16 = 0x01;
    /// Load scratch word `M[k]` into X.
    pub const LDX_MEM: u16 = 0x61;
    /// Load the input length into X.
    pub const LDX_LEN: u16 = 0x81;
    /// Load `4 * (input[k] & 0x0f)` into X.
    pub const LDX_B_MSH: u16 = 0xb1;

    /// Store A into scratch word `M[k]`.
    pub const ST: u16 = 0x02;
    /// Store X into scratch word `M[k]`.
    pub const STX: u16 = 0x03;

    /// Add an immediate to A.
    pub const ALU_ADD_K: u16 = 0x04;
    /// Add X to A.
    pub const ALU_ADD_X: u16 = 0x0c;
    /// Subtract an immediate from A.
    pub const ALU_SUB_K: u16 = 0x14;
    /// Subtract X from A.
    pub const ALU_SUB_X: u16 = 0x1c;
    /// Multiply A by an immediate.
    pub const ALU_MUL_K: u16 = 0x24;
    /// Multiply A by X.
    pub const ALU_MUL_X: u16 = 0x2c;
    /// Divide A by a nonzero immediate.
    pub const ALU_DIV_K: u16 = 0x34;
    /// Divide A by X, terminating with zero when X is zero.
    pub const ALU_DIV_X: u16 = 0x3c;
    /// Bitwise-OR A with an immediate.
    pub const ALU_OR_K: u16 = 0x44;
    /// Bitwise-OR A with X.
    pub const ALU_OR_X: u16 = 0x4c;
    /// Bitwise-AND A with an immediate.
    pub const ALU_AND_K: u16 = 0x54;
    /// Bitwise-AND A with X.
    pub const ALU_AND_X: u16 = 0x5c;
    /// Shift A left by an immediate in `0..32`.
    pub const ALU_LSH_K: u16 = 0x64;
    /// Shift A left by the low five bits of X.
    pub const ALU_LSH_X: u16 = 0x6c;
    /// Shift A right by an immediate in `0..32`.
    pub const ALU_RSH_K: u16 = 0x74;
    /// Shift A right by the low five bits of X.
    pub const ALU_RSH_X: u16 = 0x7c;
    /// Replace A with its two's-complement negation.
    pub const ALU_NEG: u16 = 0x84;
    /// Replace A with the remainder after a nonzero immediate divisor.
    pub const ALU_MOD_K: u16 = 0x94;
    /// Replace A with the remainder after X, terminating with zero when X is zero.
    pub const ALU_MOD_X: u16 = 0x9c;
    /// Bitwise-XOR A with an immediate.
    pub const ALU_XOR_K: u16 = 0xa4;
    /// Bitwise-XOR A with X.
    pub const ALU_XOR_X: u16 = 0xac;

    /// Jump forward unconditionally by `k` instructions after the next one.
    pub const JMP_JA: u16 = 0x05;
    /// Jump according to whether A equals an immediate.
    pub const JMP_JEQ_K: u16 = 0x15;
    /// Jump according to whether A equals X.
    pub const JMP_JEQ_X: u16 = 0x1d;
    /// Jump according to whether A is greater than an immediate.
    pub const JMP_JGT_K: u16 = 0x25;
    /// Jump according to whether A is greater than X.
    pub const JMP_JGT_X: u16 = 0x2d;
    /// Jump according to whether A is at least an immediate.
    pub const JMP_JGE_K: u16 = 0x35;
    /// Jump according to whether A is at least X.
    pub const JMP_JGE_X: u16 = 0x3d;
    /// Jump according to whether A and an immediate share a set bit.
    pub const JMP_JSET_K: u16 = 0x45;
    /// Jump according to whether A and X share a set bit.
    pub const JMP_JSET_X: u16 = 0x4d;

    /// Return the immediate in `k`.
    pub const RET_K: u16 = 0x06;
    /// Return A.
    pub const RET_A: u16 = 0x16;

    /// Copy A into X.
    pub const MISC_TAX: u16 = 0x07;
    /// Copy X into A.
    pub const MISC_TXA: u16 = 0x87;
}

/// One classic-BPF instruction in the standard eight-byte wire layout.
///
/// `jt` and `jf` are relative offsets from the instruction following a
/// conditional jump. `k` is an immediate, scratch index, load offset, or
/// unconditional jump offset depending on `code`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Instruction {
    /// Encoded operation.
    pub code: u16,
    /// Conditional true-branch offset.
    pub jt: u8,
    /// Conditional false-branch offset.
    pub jf: u8,
    /// Operation-specific immediate value.
    pub k: u32,
}

impl Instruction {
    /// Constructs one instruction with every raw UAPI field specified exactly.
    pub const fn new(code: u16, jt: u8, jf: u8, k: u32) -> Self {
        Self { code, jt, jf, k }
    }

    /// Constructs a non-conditional instruction.
    pub const fn statement(code: u16, k: u32) -> Self {
        Self::new(code, 0, 0, k)
    }

    /// Constructs a conditional jump instruction.
    pub const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Self {
        Self::new(code, jt, jf, k)
    }
}
