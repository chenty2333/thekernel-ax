#![cfg(all(unix, target_arch = "x86_64"))]

use std::ffi::c_void;
use std::ptr;

use axcbpf::{
    Ancillary, ImageValidationError, InputProfile, Instruction, NativeWordInput, PacketInput,
    PacketInputContext, PacketMetadata, Program, TranslationValidator, opcode,
    validate_translation_bytes,
};

unsafe extern "C" {
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: i32,
        flags: i32,
        file: i32,
        offset: isize,
    ) -> *mut c_void;
    fn mprotect(address: *mut c_void, length: usize, protection: i32) -> i32;
    fn munmap(address: *mut c_void, length: usize) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;

type JitFn = extern "C" fn(*const u8, u32) -> u32;

fn statement(code: u16, k: u32) -> Instruction {
    Instruction::statement(code, k)
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Instruction {
    Instruction::jump(code, k, jt, jf)
}

#[test]
fn emits_real_machine_code_with_packet_profile() {
    let program = Program::verify(&[
        statement(opcode::LD_B_ABS, 0),
        jump(opcode::JMP_JEQ_K, 0x45, 0, 1),
        statement(opcode::RET_K, 1),
        statement(opcode::RET_K, 0),
    ])
    .unwrap();
    let image = program.translate().unwrap();
    assert_eq!(image.entry(), 0);
    assert_eq!(image.profile(), InputProfile::PacketBytesBigEndian);
    assert_eq!(image.relocations(), &[]);
    assert!(image.instruction_boundaries().first().copied().unwrap() > image.entry());
    assert_eq!(
        image.instruction_map().first().unwrap().offset,
        image.instruction_boundaries().first().copied().unwrap()
    );
    assert_eq!(
        image.instruction_boundaries().last(),
        Some(&(image.bytes().len() as u32))
    );
    TranslationValidator::validate(&image).unwrap();
    assert_eq!(run(image.bytes(), &[0x45]), 1);
    assert_eq!(run(image.bytes(), &[0x44]), 0);
    assert_eq!(image.evaluate(&[0x45][..]), program.evaluate(&[0x45][..]));
}

#[test]
fn packet_context_ancillary_lowering_matches_interpreter_and_validator() {
    let metadata = PacketMetadata::new(0x0800, 9, 1, 0xfeed_beef, 5, 0x0064, true, 0x8100);
    let packet = [0xde, 0xad, 0xbe, 0xef];
    let input = PacketInput::new(&packet, metadata);
    for (field, expected) in [
        (Ancillary::Protocol, 0x0800),
        (Ancillary::Ifindex, 9),
        (Ancillary::Pkttype, 1),
        (Ancillary::Mark, 0xfeed_beef),
        (Ancillary::Queue, 5),
        (Ancillary::VlanTag, 0x0064),
        (Ancillary::VlanTagPresent, 1),
        (Ancillary::VlanTpid, 0x8100),
    ] {
        for code in [opcode::LD_W_ABS, opcode::LD_H_ABS, opcode::LD_B_ABS] {
            let program = Program::verify(&[
                statement(code, field.encoded_offset()),
                statement(opcode::RET_A, 0),
            ])
            .unwrap();
            let image = program
                .translate_with_profile(InputProfile::PacketContextBigEndian)
                .unwrap();
            validate_translation_bytes(
                image.bytes(),
                program.instructions(),
                InputProfile::PacketContextBigEndian,
            )
            .unwrap();
            assert_eq!(program.evaluate(&input), expected, "{field:?}/{code:#x}");
            assert_eq!(image.evaluate(&input), expected, "{field:?}/{code:#x}");
            assert_eq!(
                run_context(image.bytes(), &packet, metadata),
                expected,
                "{field:?}/{code:#x}"
            );
        }
    }

    let ordinary =
        Program::verify(&[statement(opcode::LD_W_ABS, 0), statement(opcode::RET_A, 0)]).unwrap();
    let ordinary_image = ordinary
        .translate_with_profile(InputProfile::PacketContextBigEndian)
        .unwrap();
    assert_eq!(
        run_context(ordinary_image.bytes(), &packet, metadata),
        ordinary.evaluate(&input)
    );
}

#[test]
fn host_executable_differential_for_generated_programs() {
    let mut state = 0x1234_5678_u32;
    for round in 0..256 {
        let program = Program::verify(&[
            statement(opcode::LD_IMM, next(&mut state)),
            statement(opcode::LDX_IMM, next(&mut state)),
            statement(opcode::ALU_ADD_X, 0),
            statement(opcode::ALU_XOR_K, next(&mut state)),
            statement(opcode::ALU_LSH_K, next(&mut state) & 31),
            statement(opcode::ALU_NEG, 0),
            statement(opcode::RET_A, 0),
        ])
        .unwrap();
        let image = program.translate().unwrap();
        for input_round in 0..4 {
            let input = [round as u8, input_round, 0xa5, 0x5a];
            assert_eq!(run(image.bytes(), &input), program.evaluate(&input[..]));
            assert_eq!(image.evaluate(&input[..]), program.evaluate(&input[..]));
        }
    }
}

#[test]
fn packet_loads_and_native_aligned_word_profile_execute() {
    let packet = Program::verify(&[
        statement(opcode::LD_W_ABS, 0),
        statement(opcode::ST, 0),
        statement(opcode::LD_H_ABS, 4),
        statement(opcode::ALU_XOR_K, 0x1122),
        statement(opcode::LD_B_ABS, 6),
        statement(opcode::ALU_ADD_K, 1),
        statement(opcode::LD_MEM, 0),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let packet_image = packet.translate().unwrap();
    let bytes = [0x01, 0x02, 0x03, 0x04, 0x11, 0x22, 0x7f, 0x00];
    assert_eq!(
        run(packet_image.bytes(), &bytes),
        packet.evaluate(&bytes[..])
    );

    let native =
        Program::verify(&[statement(opcode::LD_W_ABS, 4), statement(opcode::RET_A, 0)]).unwrap();
    let native_image = native
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let native_bytes = [0, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
    let native_input = NativeWordInput::new(&native_bytes);
    assert_eq!(
        run(native_image.bytes(), &native_bytes),
        native.evaluate(&native_input)
    );
    assert_eq!(
        native_image.evaluate(&native_input),
        native.evaluate(&native_input)
    );
}

#[test]
fn half_load_replaces_the_full_accumulator_for_packet_abs_and_ind() {
    let absolute = Program::verify(&[
        statement(opcode::LD_IMM, 0xfeed_0000),
        statement(opcode::LD_H_ABS, 2),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let absolute_image = absolute.translate().unwrap();
    let absolute_input = [0x00, 0x01, 0x12, 0x34, 0xa5];
    let absolute_expected = absolute.evaluate(&absolute_input[..]);
    assert_eq!(absolute_expected, 0x1234);
    assert_eq!(
        run(absolute_image.bytes(), &absolute_input),
        absolute_expected
    );
    assert_eq!(
        absolute_image.evaluate(&absolute_input[..]),
        absolute_expected
    );

    let indirect = Program::verify(&[
        statement(opcode::LD_IMM, 0xcafe_0000),
        statement(opcode::LDX_IMM, 1),
        statement(opcode::LD_H_IND, 2),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let indirect_image = indirect.translate().unwrap();
    let indirect_input = [0x00, 0x01, 0x02, 0xab, 0xcd, 0xef];
    let indirect_expected = indirect.evaluate(&indirect_input[..]);
    assert_eq!(indirect_expected, 0xabcd);
    assert_eq!(
        run(indirect_image.bytes(), &indirect_input),
        indirect_expected
    );
    assert_eq!(
        indirect_image.evaluate(&indirect_input[..]),
        indirect_expected
    );
}

#[test]
fn ldx_b_msh_updates_x_without_clobbering_a() {
    let program = Program::verify(&[
        statement(opcode::LD_IMM, 0xa5a5_5a5a),
        statement(opcode::LDX_B_MSH, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let image = program.translate().unwrap();
    let input = [0x0b];

    // The non-zero accumulator makes the old lowering (load into EAX, then
    // copy to ECX) observably wrong.  Compare both the executable image and
    // its safe source model with the canonical interpreter.
    let expected = program.evaluate(&input[..]);
    assert_eq!(expected, 0xa5a5_5a5a);
    assert_eq!(image.evaluate(&input[..]), expected);
    assert_eq!(run(image.bytes(), &input), expected);
}

#[test]
fn native_profile_loads_also_replace_the_full_accumulator() {
    let program = Program::verify(&[
        statement(opcode::LD_IMM, 0xdead_0000),
        statement(opcode::LD_W_ABS, 4),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let image = program
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let input = [0, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
    let native_input = NativeWordInput::new(&input);
    let expected = program.evaluate(&native_input);
    assert_eq!(expected, 0x1234_5678);
    assert_eq!(run(image.bytes(), &input), expected);
    assert_eq!(image.evaluate(&native_input), expected);

    let unsupported_half =
        Program::verify(&[statement(opcode::LD_H_ABS, 0), statement(opcode::RET_A, 0)]).unwrap();
    assert!(
        unsupported_half
            .translate_with_profile(InputProfile::NativeAlignedWords)
            .is_err()
    );
}

#[test]
fn native_profile_rejects_non_word_loads() {
    let program =
        Program::verify(&[statement(opcode::LD_B_ABS, 0), statement(opcode::RET_A, 0)]).unwrap();
    assert!(
        program
            .translate_with_profile(InputProfile::NativeAlignedWords)
            .is_err()
    );
}

#[test]
fn direct_emitter_covers_x_sources_scratch_indirect_and_jset() {
    let program = Program::verify(&[
        statement(opcode::LD_IMM, 0x55),
        statement(opcode::LDX_IMM, 3),
        statement(opcode::ST, 0),
        statement(opcode::STX, 1),
        statement(opcode::LD_MEM, 0),
        statement(opcode::ALU_ADD_X, 0),
        statement(opcode::ALU_DIV_X, 0),
        statement(opcode::ALU_MOD_X, 0),
        statement(opcode::ALU_LSH_X, 0),
        statement(opcode::ALU_RSH_X, 0),
        statement(opcode::LDX_MEM, 1),
        statement(opcode::ALU_XOR_X, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let image = program.translate().unwrap();
    assert_eq!(run(image.bytes(), &[]), program.evaluate(&[][..]));

    for (code, expected) in [(opcode::JMP_JSET_X, 1), (opcode::JMP_JSET_K, 1)] {
        let branch = Program::verify(&[
            statement(opcode::LD_IMM, 4),
            statement(opcode::LDX_IMM, 4),
            jump(code, 4, 0, 1),
            statement(opcode::RET_K, expected),
            statement(opcode::RET_K, 0),
        ])
        .unwrap();
        let image = branch.translate().unwrap();
        assert_eq!(run(image.bytes(), &[]), expected);
    }

    let indirect = Program::verify(&[
        statement(opcode::LDX_IMM, 2),
        statement(opcode::LD_W_IND, 1),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let image = indirect.translate().unwrap();
    let input = [0, 0, 0, 0x12, 0x34, 0x56, 0x78];
    assert_eq!(run(image.bytes(), &input), indirect.evaluate(&input[..]));
}

#[test]
fn native_profile_checks_runtime_indirect_alignment() {
    let program = Program::verify(&[
        statement(opcode::LDX_IMM, 1),
        statement(opcode::LD_W_IND, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let image = program
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let input = [0, 1, 2, 3, 4, 5, 6, 7];
    let native_input = NativeWordInput::new(&input);
    assert_eq!(run(image.bytes(), &input), 0);
    assert_eq!(image.evaluate(&native_input), 0);
}

#[test]
fn direct_load_bounds_and_offset_overflow_fail_zero() {
    let short =
        Program::verify(&[statement(opcode::LD_W_ABS, 1), statement(opcode::RET_K, 9)]).unwrap();
    let short_image = short.translate().unwrap();
    assert_eq!(run(short_image.bytes(), &[1, 2, 3]), 0);

    let overflow = Program::verify(&[
        statement(opcode::LDX_IMM, u32::MAX),
        statement(opcode::LD_B_IND, 1),
        statement(opcode::RET_K, 9),
    ])
    .unwrap();
    let overflow_image = overflow.translate().unwrap();
    assert_eq!(run(overflow_image.bytes(), &[1]), 0);
}

#[test]
fn direct_packet_loads_have_independent_lfence_validation() {
    for source in [
        vec![statement(opcode::LD_B_ABS, 0), statement(opcode::RET_A, 0)],
        vec![
            statement(opcode::LDX_IMM, 0),
            statement(opcode::LD_B_IND, 0),
            statement(opcode::RET_A, 0),
        ],
    ] {
        let program = Program::verify(&source).unwrap();
        let image = program.translate().unwrap();
        assert!(
            image
                .bytes()
                .windows(3)
                .any(|window| window == [0x0f, 0xae, 0xe8]),
            "direct packet load must contain x86 LFENCE"
        );
        validate_translation_bytes(image.bytes(), program.instructions(), image.profile()).unwrap();

        let mut without_lfence = image.bytes().to_vec();
        replace_bytes(
            &mut without_lfence,
            &[0x0f, 0xae, 0xe8],
            &[0x0f, 0xae, 0xe9],
        );
        assert!(
            validate_translation_bytes(&without_lfence, program.instructions(), image.profile())
                .is_err(),
            "independent validator must reject a missing LFENCE sequence"
        );
    }
}

#[test]
fn translation_validator_covers_every_supported_source_lowering() {
    let mut cases = vec![
        vec![statement(opcode::LD_IMM, 7), statement(opcode::RET_A, 0)],
        vec![statement(opcode::LD_W_ABS, 0), statement(opcode::RET_A, 0)],
        vec![statement(opcode::LD_H_ABS, 0), statement(opcode::RET_A, 0)],
        vec![statement(opcode::LD_B_ABS, 0), statement(opcode::RET_A, 0)],
        vec![
            statement(opcode::LDX_IMM, 0),
            statement(opcode::LD_W_IND, 0),
            statement(opcode::RET_A, 0),
        ],
        vec![
            statement(opcode::LDX_IMM, 0),
            statement(opcode::LD_H_IND, 0),
            statement(opcode::RET_A, 0),
        ],
        vec![
            statement(opcode::LDX_IMM, 0),
            statement(opcode::LD_B_IND, 0),
            statement(opcode::RET_A, 0),
        ],
        vec![
            statement(opcode::LD_IMM, 7),
            statement(opcode::ST, 0),
            statement(opcode::LD_MEM, 0),
            statement(opcode::RET_A, 0),
        ],
        vec![
            statement(opcode::LDX_IMM, 7),
            statement(opcode::STX, 0),
            statement(opcode::LDX_MEM, 0),
            statement(opcode::RET_A, 0),
        ],
        vec![statement(opcode::LD_LEN, 0), statement(opcode::RET_A, 0)],
        vec![statement(opcode::LDX_LEN, 0), statement(opcode::RET_A, 0)],
        vec![statement(opcode::LDX_B_MSH, 0), statement(opcode::RET_A, 0)],
        vec![statement(opcode::MISC_TAX, 0), statement(opcode::RET_A, 0)],
        vec![
            statement(opcode::LDX_IMM, 7),
            statement(opcode::MISC_TXA, 0),
            statement(opcode::RET_A, 0),
        ],
    ];
    for code in [
        opcode::ALU_ADD_K,
        opcode::ALU_SUB_K,
        opcode::ALU_MUL_K,
        opcode::ALU_DIV_K,
        opcode::ALU_OR_K,
        opcode::ALU_AND_K,
        opcode::ALU_LSH_K,
        opcode::ALU_RSH_K,
        opcode::ALU_NEG,
        opcode::ALU_MOD_K,
        opcode::ALU_XOR_K,
    ] {
        cases.push(vec![
            statement(opcode::LD_IMM, 7),
            statement(
                code,
                if matches!(code, opcode::ALU_DIV_K | opcode::ALU_MOD_K) {
                    3
                } else {
                    1
                },
            ),
            statement(opcode::RET_A, 0),
        ]);
    }
    for code in [
        opcode::ALU_ADD_X,
        opcode::ALU_SUB_X,
        opcode::ALU_MUL_X,
        opcode::ALU_DIV_X,
        opcode::ALU_OR_X,
        opcode::ALU_AND_X,
        opcode::ALU_LSH_X,
        opcode::ALU_RSH_X,
        opcode::ALU_MOD_X,
        opcode::ALU_XOR_X,
    ] {
        cases.push(vec![
            statement(opcode::LD_IMM, 7),
            statement(opcode::LDX_IMM, 3),
            statement(code, 0),
            statement(opcode::RET_A, 0),
        ]);
    }
    cases.push(vec![statement(opcode::RET_K, 7)]);
    cases.push(vec![statement(opcode::RET_A, 0)]);
    for code in [
        opcode::JMP_JEQ_K,
        opcode::JMP_JEQ_X,
        opcode::JMP_JGT_K,
        opcode::JMP_JGT_X,
        opcode::JMP_JGE_K,
        opcode::JMP_JGE_X,
        opcode::JMP_JSET_K,
        opcode::JMP_JSET_X,
    ] {
        cases.push(vec![
            statement(opcode::LD_IMM, 7),
            statement(opcode::LDX_IMM, 7),
            jump(code, 7, 0, 1),
            statement(opcode::RET_K, 1),
            statement(opcode::RET_K, 0),
        ]);
    }
    cases.push(vec![
        statement(opcode::JMP_JA, 1),
        statement(opcode::RET_K, 0),
        statement(opcode::RET_K, 1),
    ]);

    for (case_index, source) in cases.into_iter().enumerate() {
        let program = Program::verify(&source).unwrap();
        let image = program
            .translate()
            .unwrap_or_else(|error| panic!("case {case_index} translation: {error:?}"));
        validate_translation_bytes(image.bytes(), program.instructions(), image.profile())
            .unwrap_or_else(|error| panic!("case {case_index} validation: {error:?}"));
        let input = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];
        assert_eq!(
            run(image.bytes(), &input),
            program.evaluate(&input[..]),
            "case {case_index}"
        );
    }
}

#[test]
fn independent_validator_rejects_native_byte_mutations() {
    let half = Program::verify(&[
        statement(opcode::LD_IMM, 0xfeed_0000),
        statement(opcode::LD_H_ABS, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let half_image = half.translate().unwrap();
    validate_translation_bytes(
        half_image.bytes(),
        half.instructions(),
        half_image.profile(),
    )
    .unwrap();

    let mut width = half_image.bytes().to_vec();
    replace_bytes(
        &mut width,
        &[0x41, 0x0f, 0xb7, 0x06],
        &[0x41, 0x0f, 0xb6, 0x06],
    );
    assert!(validate_translation_bytes(&width, half.instructions(), half_image.profile()).is_err());

    let mut old_half = half_image.bytes().to_vec();
    insert_before(&mut old_half, &[0x41, 0x0f, 0xb7, 0x06], 0x66);
    assert!(
        validate_translation_bytes(&old_half, half.instructions(), half_image.profile()).is_err()
    );

    let mut load_register = half_image.bytes().to_vec();
    replace_bytes(
        &mut load_register,
        &[0x41, 0x0f, 0xb7, 0x06],
        &[0x41, 0x0f, 0xb7, 0x0e],
    );
    assert!(
        validate_translation_bytes(&load_register, half.instructions(), half_image.profile())
            .is_err()
    );

    let mut bounds = half_image.bytes().to_vec();
    replace_bytes(&mut bounds, &[0x0f, 0x82], &[0x0f, 0x83]);
    assert!(
        validate_translation_bytes(&bounds, half.instructions(), half_image.profile()).is_err()
    );

    let scratch = Program::verify(&[
        statement(opcode::LD_IMM, 7),
        statement(opcode::ST, 0),
        statement(opcode::LD_MEM, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let scratch_image = scratch.translate().unwrap();
    let mut scratch_register = scratch_image.bytes().to_vec();
    replace_bytes(
        &mut scratch_register,
        &[0x89, 0x45, 0xfc],
        &[0x89, 0x4d, 0xfc],
    );
    assert!(
        validate_translation_bytes(
            &scratch_register,
            scratch.instructions(),
            scratch_image.profile()
        )
        .is_err()
    );

    let division = Program::verify(&[
        statement(opcode::LD_IMM, 7),
        statement(opcode::ALU_DIV_K, 3),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let division_image = division.translate().unwrap();
    let mut divisor_register = division_image.bytes().to_vec();
    replace_bytes(
        &mut divisor_register,
        &[0x41, 0xf7, 0xf6],
        &[0x41, 0xf7, 0xf1],
    );
    assert!(
        validate_translation_bytes(
            &divisor_register,
            division.instructions(),
            division_image.profile()
        )
        .is_err()
    );

    let branch = Program::verify(&[
        statement(opcode::LD_IMM, 7),
        jump(opcode::JMP_JEQ_K, 7, 0, 1),
        statement(opcode::RET_K, 1),
        statement(opcode::RET_K, 0),
    ])
    .unwrap();
    let branch_image = branch.translate().unwrap();
    let mut branch_condition = branch_image.bytes().to_vec();
    replace_bytes(&mut branch_condition, &[0x0f, 0x84], &[0x0f, 0x85]);
    assert!(
        validate_translation_bytes(
            &branch_condition,
            branch.instructions(),
            branch_image.profile()
        )
        .is_err()
    );

    let mut branch_target = branch_image.bytes().to_vec();
    let branch_opcode = branch_target
        .windows(2)
        .position(|window| window == [0x0f, 0x84])
        .unwrap();
    branch_target[branch_opcode + 2] ^= 1;
    assert!(
        validate_translation_bytes(
            &branch_target,
            branch.instructions(),
            branch_image.profile()
        )
        .is_err()
    );

    let mut prologue = branch_image.bytes().to_vec();
    replace_bytes(
        &mut prologue,
        &[0x48, 0x83, 0xec, 0x40],
        &[0x48, 0x83, 0xec, 0x20],
    );
    assert!(
        validate_translation_bytes(&prologue, branch.instructions(), branch_image.profile())
            .is_err()
    );

    let truncated = &branch_image.bytes()[..branch_image.bytes().len() - 1];
    assert!(
        validate_translation_bytes(truncated, branch.instructions(), branch_image.profile())
            .is_err()
    );

    let mut tail = branch_image.bytes().to_vec();
    tail.push(0x90);
    assert!(
        validate_translation_bytes(&tail, branch.instructions(), branch_image.profile()).is_err()
    );

    // The source trace is intentionally checked against every byte of an
    // image, not only a handful of opcode/displacement sites.  This catches
    // a decoder that accidentally accepts an alternate encoding or ignores a
    // trailing immediate byte.
    for offset in 0..branch_image.bytes().len() {
        let mut mutated = branch_image.bytes().to_vec();
        mutated[offset] ^= 1;
        assert!(
            validate_translation_bytes(&mutated, branch.instructions(), branch_image.profile())
                .is_err(),
            "single-byte mutation at {offset} was accepted"
        );
    }

    let mut unsupported = branch_image.bytes().to_vec();
    unsupported[0] = 0x90; // NOP is outside the closed translator subset.
    let unsupported_error =
        validate_translation_bytes(&unsupported, branch.instructions(), branch_image.profile())
            .expect_err("NOP must be rejected by the restricted decoder");
    assert!(matches!(
        unsupported_error,
        ImageValidationError::NativeDecode { offset: 0 }
    ));
}

#[test]
fn independent_source_contract_rejects_widened_or_invalid_inputs() {
    // The bytes are deliberately from an otherwise valid two-instruction
    // image.  The validator must reject the independently supplied source
    // contract before attempting to match those bytes to it.
    let valid =
        Program::verify(&[statement(opcode::LD_IMM, 7), statement(opcode::RET_A, 0)]).unwrap();
    let image = valid.translate().unwrap();

    let unsupported = [statement(0xffff, 0), statement(opcode::RET_K, 0)];
    assert_eq!(
        validate_translation_bytes(image.bytes(), &unsupported, image.profile()),
        Err(ImageValidationError::UnsupportedOpcode {
            pc: 0,
            code: 0xffff
        })
    );

    let bad_jump = [statement(opcode::JMP_JA, 1), statement(opcode::RET_K, 0)];
    assert_eq!(
        validate_translation_bytes(image.bytes(), &bad_jump, image.profile()),
        Err(ImageValidationError::JumpOutOfRange { pc: 0 })
    );

    let uninitialized_scratch = [statement(opcode::LD_MEM, 0), statement(opcode::RET_A, 0)];
    assert_eq!(
        validate_translation_bytes(image.bytes(), &uninitialized_scratch, image.profile()),
        Err(ImageValidationError::ScratchUninitialized { pc: 0, index: 0 })
    );

    let native_byte_load = [statement(opcode::LD_B_ABS, 0), statement(opcode::RET_A, 0)];
    assert_eq!(
        validate_translation_bytes(
            image.bytes(),
            &native_byte_load,
            InputProfile::NativeAlignedWords
        ),
        Err(ImageValidationError::ProfileUnsupported {
            pc: 0,
            code: opcode::LD_B_ABS,
        })
    );

    let unaligned_native_word = [statement(opcode::LD_W_ABS, 2), statement(opcode::RET_A, 0)];
    assert_eq!(
        validate_translation_bytes(
            image.bytes(),
            &unaligned_native_word,
            InputProfile::NativeAlignedWords
        ),
        Err(ImageValidationError::ProfileUnsupported {
            pc: 0,
            code: opcode::LD_W_ABS,
        })
    );

    let ancillary = [
        statement(opcode::LD_W_ABS, 0x8000_0000),
        statement(opcode::RET_A, 0),
    ];
    assert_eq!(
        validate_translation_bytes(image.bytes(), &ancillary, image.profile()),
        Err(ImageValidationError::UnsupportedAncillaryLoad {
            pc: 0,
            offset: 0x8000_0000,
        })
    );
}

#[test]
fn native_profile_alignment_is_logical_not_slice_address_alignment() {
    let absolute =
        Program::verify(&[statement(opcode::LD_W_ABS, 0), statement(opcode::RET_A, 0)]).unwrap();
    let absolute_image = absolute
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let mut storage = [0u8; 12];
    let unaligned_offset = (0..4)
        .map(|offset| storage[offset..].as_ptr() as usize)
        .position(|address| address & 3 != 0)
        .expect("at least one byte offset must be unaligned");
    storage[unaligned_offset..unaligned_offset + 4].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
    let unaligned_base = &storage[unaligned_offset..];
    let absolute_input = NativeWordInput::new(unaligned_base);
    assert!(!absolute_input.base_is_aligned());
    assert!(NativeWordInput::new_aligned(unaligned_base).is_none());
    assert_eq!(absolute.evaluate(&absolute_input), 0x1234_5678);
    assert_eq!(run(absolute_image.bytes(), unaligned_base), 0x1234_5678);

    let indirect = Program::verify(&[
        statement(opcode::LDX_IMM, 1),
        statement(opcode::LD_W_IND, 0),
        statement(opcode::RET_A, 0),
    ])
    .unwrap();
    let indirect_image = indirect
        .translate_with_profile(InputProfile::NativeAlignedWords)
        .unwrap();
    let indirect_input = NativeWordInput::new(unaligned_base);
    assert_eq!(indirect.evaluate(&indirect_input), 0);
    assert_eq!(run(indirect_image.bytes(), unaligned_base), 0);
}

fn next(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

fn replace_bytes(bytes: &mut [u8], needle: &[u8], replacement: &[u8]) {
    assert_eq!(needle.len(), replacement.len());
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("needle not present");
    bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
}

fn insert_before(bytes: &mut Vec<u8>, needle: &[u8], value: u8) {
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("needle not present");
    bytes.insert(offset, value);
}

fn run(bytes: &[u8], input: &[u8]) -> u32 {
    let page_size = 4096;
    let mapping = unsafe {
        mmap(
            ptr::null_mut(),
            page_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(mapping as isize, -1, "mmap failed");
    assert!(bytes.len() <= page_size);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.cast::<u8>(), bytes.len());
        assert_eq!(mprotect(mapping, page_size, PROT_READ | PROT_EXEC), 0);
        let function: JitFn = std::mem::transmute(mapping);
        let result = function(input.as_ptr(), input.len() as u32);
        assert_eq!(mprotect(mapping, page_size, PROT_READ | PROT_WRITE), 0);
        assert_eq!(munmap(mapping, page_size), 0);
        result
    }
}

fn run_context(bytes: &[u8], packet: &[u8], metadata: PacketMetadata) -> u32 {
    let context = PacketInputContext::new(packet, metadata);
    // The public JIT image intentionally remains a small two-argument native
    // ABI.  This test supplies the packet-aware profile's typed context as
    // its first argument; the context lives until the synchronous call
    // returns, and the executable mapping is kept live by `run`.
    let context_bytes = unsafe {
        std::slice::from_raw_parts(
            (&context as *const PacketInputContext).cast::<u8>(),
            std::mem::size_of::<PacketInputContext>(),
        )
    };
    run(bytes, context_bytes)
}
