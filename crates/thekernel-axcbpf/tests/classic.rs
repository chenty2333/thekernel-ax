use core::mem::{align_of, offset_of, size_of};

use axcbpf::{Input, Instruction, LoadWidth, Program, VerifyError, opcode};

fn statement(code: u16, k: u32) -> Instruction {
    Instruction::statement(code, k)
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Instruction {
    Instruction::jump(code, k, jt, jf)
}

fn program(instructions: &[Instruction]) -> Program {
    Program::verify(instructions).unwrap()
}

#[test]
fn instruction_has_standard_wire_layout() {
    assert_eq!(
        Instruction::new(0x1234, 0x56, 0x78, 0x9abc_def0),
        Instruction {
            code: 0x1234,
            jt: 0x56,
            jf: 0x78,
            k: 0x9abc_def0,
        }
    );
    assert_eq!(size_of::<Instruction>(), 8);
    assert_eq!(align_of::<Instruction>(), 4);
    assert_eq!(offset_of!(Instruction, code), 0);
    assert_eq!(offset_of!(Instruction, jt), 2);
    assert_eq!(offset_of!(Instruction, jf), 3);
    assert_eq!(offset_of!(Instruction, k), 4);
}

#[test]
fn existing_instruction_vector_is_adopted_without_copying() {
    let mut instructions = Vec::new();
    instructions.try_reserve_exact(2).unwrap();
    instructions.push(statement(opcode::LD_IMM, 7));
    instructions.push(statement(opcode::RET_A, 0));
    let allocation = instructions.as_ptr();

    let filter = Program::try_from_vec(instructions).unwrap();
    assert_eq!(filter.instructions().as_ptr(), allocation);
    assert_eq!(filter.evaluate(&[][..]), 7);
}

#[test]
fn byte_slice_loads_network_order_values() {
    let input = [0x01, 0x23, 0x45, 0x67, 0x89];
    assert_eq!(Input::len(&input[..]), 5);
    assert_eq!(input[..].load(1, LoadWidth::Byte), Some(0x23));
    assert_eq!(input[..].load(1, LoadWidth::Half), Some(0x2345));
    assert_eq!(input[..].load(1, LoadWidth::Word), Some(0x2345_6789));
    assert_eq!(input[..].load(2, LoadWidth::Word), None);
}

#[test]
fn rejects_empty_and_oversized_programs() {
    assert_eq!(Program::verify(&[]).unwrap_err(), VerifyError::Empty);

    let oversized = vec![statement(opcode::RET_K, 0); 4097];
    assert_eq!(
        Program::verify(&oversized).unwrap_err(),
        VerifyError::TooLong { length: 4097 }
    );
}

#[test]
fn rejects_unsupported_opcode_and_ancillary_offset() {
    assert_eq!(
        Program::verify(&[statement(0xffff, 0), statement(opcode::RET_K, 0)]).unwrap_err(),
        VerifyError::UnsupportedOpcode {
            pc: 0,
            code: 0xffff
        }
    );
    assert_eq!(
        Program::verify(&[
            statement(opcode::LD_W_ABS, 0xffff_f000),
            statement(opcode::RET_A, 0),
        ])
        .unwrap_err(),
        VerifyError::UnsupportedAncillaryLoad {
            pc: 0,
            offset: 0xffff_f000
        }
    );
}

#[test]
fn rejects_invalid_immediate_arithmetic() {
    for code in [opcode::ALU_DIV_K, opcode::ALU_MOD_K] {
        assert_eq!(
            Program::verify(&[statement(code, 0), statement(opcode::RET_A, 0)]).unwrap_err(),
            VerifyError::ImmediateDivisionByZero { pc: 0 }
        );
    }
    for code in [opcode::ALU_LSH_K, opcode::ALU_RSH_K] {
        assert_eq!(
            Program::verify(&[statement(code, 32), statement(opcode::RET_A, 0)]).unwrap_err(),
            VerifyError::ImmediateShiftOutOfRange { pc: 0, shift: 32 }
        );
    }
}

#[test]
fn rejects_invalid_scratch_and_jump_geometry() {
    assert_eq!(
        Program::verify(&[statement(opcode::ST, 16), statement(opcode::RET_K, 0),]).unwrap_err(),
        VerifyError::ScratchOutOfRange { pc: 0, index: 16 }
    );
    assert_eq!(
        Program::verify(&[
            statement(opcode::JMP_JA, u32::MAX),
            statement(opcode::RET_K, 0),
        ])
        .unwrap_err(),
        VerifyError::JumpOutOfRange { pc: 0 }
    );
    assert_eq!(
        Program::verify(&[
            jump(opcode::JMP_JEQ_K, 0, 2, 0),
            statement(opcode::RET_K, 0),
        ])
        .unwrap_err(),
        VerifyError::JumpOutOfRange { pc: 0 }
    );
    assert_eq!(
        Program::verify(&[statement(opcode::LD_IMM, 0)]).unwrap_err(),
        VerifyError::MissingFinalReturn
    );
}

#[test]
fn rejects_scratch_read_not_initialized_on_every_branch() {
    let error = Program::verify(&[
        jump(opcode::JMP_JEQ_K, 0, 0, 1),
        statement(opcode::ST, 3),
        statement(opcode::LD_MEM, 3),
        statement(opcode::RET_A, 0),
    ])
    .unwrap_err();
    assert_eq!(error, VerifyError::ScratchUninitialized { pc: 2, index: 3 });
}

#[test]
fn accepts_scratch_initialized_on_both_branches() {
    let filter = program(&[
        statement(opcode::LD_IMM, 11),
        jump(opcode::JMP_JEQ_K, 11, 0, 2),
        statement(opcode::ST, 3),
        statement(opcode::JMP_JA, 1),
        statement(opcode::ST, 3),
        statement(opcode::LD_MEM, 3),
        statement(opcode::RET_A, 0),
    ]);
    assert_eq!(filter.evaluate(&[][..]), 11);
}

#[test]
fn executes_absolute_indirect_length_and_msh_loads() {
    let input = [0x45, 0x00, 0x00, 0x2c, 0xaa, 0xbb, 0xcc, 0xdd];
    let filter = program(&[
        statement(opcode::LD_W_ABS, 0),
        statement(opcode::ST, 0),
        statement(opcode::LD_H_ABS, 4),
        statement(opcode::ST, 1),
        statement(opcode::LD_B_ABS, 7),
        statement(opcode::ST, 2),
        statement(opcode::LDX_IMM, 4),
        statement(opcode::LD_W_IND, 0),
        statement(opcode::ST, 3),
        statement(opcode::LDX_B_MSH, 0),
        statement(opcode::STX, 4),
        statement(opcode::LD_LEN, 0),
        statement(opcode::ST, 5),
        statement(opcode::LDX_LEN, 0),
        statement(opcode::LD_MEM, 0),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::LDX_MEM, 1),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::LDX_MEM, 2),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::LDX_MEM, 3),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::LDX_MEM, 4),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::LDX_MEM, 5),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::RET_A, 0),
    ]);

    let expected = 0x4500_002c ^ 0xaabb ^ 0xdd ^ 0xaabb_ccdd ^ 20 ^ 8 ^ 8;
    assert_eq!(filter.evaluate(&input[..]), expected);
}

#[test]
fn executes_wrapping_alu_and_register_transfers() {
    let filter = program(&[
        statement(opcode::LD_IMM, u32::MAX),
        statement(opcode::ALU_ADD_K, 2),
        statement(opcode::MISC_TAX, 0),
        statement(opcode::LD_IMM, 21),
        statement(opcode::ALU_MUL_X, 0),
        statement(opcode::ALU_SUB_K, 1),
        statement(opcode::ALU_DIV_K, 4),
        statement(opcode::ALU_MOD_K, 3),
        statement(opcode::ALU_OR_K, 8),
        statement(opcode::ALU_AND_K, 9),
        statement(opcode::ALU_XOR_K, 1),
        statement(opcode::ALU_LSH_K, 3),
        statement(opcode::LDX_IMM, 33),
        statement(opcode::ALU_RSH_X, 0),
        statement(opcode::ALU_NEG, 0),
        statement(opcode::MISC_TAX, 0),
        statement(opcode::MISC_TXA, 0),
        statement(opcode::RET_A, 0),
    ]);
    assert_eq!(filter.evaluate(&[][..]), 0xffff_ffdc);
}

#[test]
fn executes_x_source_alu_instructions() {
    let cases = [
        (opcode::ALU_ADD_X, 10, 3, 13),
        (opcode::ALU_SUB_X, 10, 3, 7),
        (opcode::ALU_MUL_X, 10, 3, 30),
        (opcode::ALU_DIV_X, 10, 3, 3),
        (opcode::ALU_MOD_X, 10, 3, 1),
        (opcode::ALU_OR_X, 0b1010, 0b0101, 0b1111),
        (opcode::ALU_AND_X, 0b1010, 0b0110, 0b0010),
        (opcode::ALU_XOR_X, 0b1010, 0b0110, 0b1100),
        (opcode::ALU_LSH_X, 3, 2, 12),
        (opcode::ALU_RSH_X, 12, 2, 3),
    ];
    for (code, accumulator, index, expected) in cases {
        let filter = program(&[
            statement(opcode::LD_IMM, accumulator),
            statement(opcode::LDX_IMM, index),
            statement(code, 0),
            statement(opcode::RET_A, 0),
        ]);
        assert_eq!(filter.evaluate(&[][..]), expected, "opcode {code:#x}");
    }
}

#[test]
fn x_shift_uses_only_the_low_five_bits() {
    for (code, accumulator, index, expected) in [
        (opcode::ALU_LSH_X, 1, 32, 1),
        (opcode::ALU_LSH_X, 1, 33, 2),
        (opcode::ALU_RSH_X, 8, 32, 8),
        (opcode::ALU_RSH_X, 8, 35, 1),
    ] {
        let filter = program(&[
            statement(opcode::LD_IMM, accumulator),
            statement(opcode::LDX_IMM, index),
            statement(code, 0),
            statement(opcode::RET_A, 0),
        ]);
        assert_eq!(filter.evaluate(&[][..]), expected);
    }
}

#[test]
fn x_division_and_modulo_by_zero_return_zero() {
    for code in [opcode::ALU_DIV_X, opcode::ALU_MOD_X] {
        let filter = program(&[
            statement(opcode::LD_IMM, 7),
            statement(opcode::LDX_IMM, 0),
            statement(code, 0),
            statement(opcode::RET_K, 99),
        ]);
        assert_eq!(filter.evaluate(&[][..]), 0);
    }
}

#[test]
fn conditional_jump_variants_choose_expected_path() {
    let cases = [
        (opcode::JMP_JEQ_K, 5, 0, 5, true),
        (opcode::JMP_JEQ_X, 5, 5, 0, true),
        (opcode::JMP_JGT_K, 6, 0, 5, true),
        (opcode::JMP_JGT_X, 6, 5, 0, true),
        (opcode::JMP_JGE_K, 5, 0, 5, true),
        (opcode::JMP_JGE_X, 5, 5, 0, true),
        (opcode::JMP_JSET_K, 4, 0, 6, true),
        (opcode::JMP_JSET_X, 4, 6, 0, true),
        (opcode::JMP_JEQ_K, 4, 0, 5, false),
    ];
    for (code, accumulator, index, immediate, expected) in cases {
        let filter = program(&[
            statement(opcode::LD_IMM, accumulator),
            statement(opcode::LDX_IMM, index),
            jump(code, immediate, 0, 1),
            statement(opcode::RET_K, 1),
            statement(opcode::RET_K, 0),
        ]);
        assert_eq!(filter.evaluate(&[][..]), u32::from(expected), "{code:#x}");
    }
}

#[test]
fn load_failure_and_indirect_overflow_terminate_with_zero() {
    let short = program(&[statement(opcode::LD_W_ABS, 1), statement(opcode::RET_K, 9)]);
    assert_eq!(short.evaluate(&[1, 2, 3][..]), 0);

    let overflow = program(&[
        statement(opcode::LDX_IMM, u32::MAX),
        statement(opcode::LD_B_IND, 1),
        statement(opcode::RET_K, 9),
    ]);
    assert_eq!(overflow.evaluate(&[1][..]), 0);
}
