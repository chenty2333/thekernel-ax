# thekernel-axcbpf

`thekernel-axcbpf` is a `no_std`, unsafe-free classic-BPF verifier and
interpreter for operating-system mechanism layers. It accepts the ordinary
classic instruction set, including packet-style absolute and indirect loads,
sixteen scratch words, forward jumps, and 32-bit arithmetic. It deliberately
does not implement Linux seccomp actions, syscall metadata, socket ownership,
errno, signals, or ancillary packet extensions.

## Contract

`Program::verify` rejects empty or over-4096-instruction programs, unsupported
opcodes, immediate division by zero, oversized immediate shifts, invalid
scratch indices, out-of-range jumps, missing final returns, and scratch loads
that are not initialized on every reachable path. Validation and the immutable
program copy use fallible allocation. `Program::try_from_vec` validates and
takes an adapter's existing instruction vector without a second instruction
allocation or copy. A verified program exposes no mutable instruction storage.

Evaluation initializes A, X, and scratch storage for every invocation and
allocates nothing. Every accepted branch moves forward, so execution is
bounded by the verified instruction count. Register-sourced shift counts use
their low five bits, matching 32-bit classic-BPF behavior. Input access is
expressed through the `Input` trait. The trait owns byte order and
domain-specific range rules; an absent load terminates with return value zero.
The built-in `[u8]` input uses network-byte-order halfword and word loads,
which is useful for ordinary socket-filter data. A seccomp adapter can instead
expose native-endian aligned words from its immutable syscall snapshot.

Negative encoded absolute offsets are rejected rather than interpreted as
Linux `SKF_AD_*`, link-layer, or network-layer ancillary extensions. A future
socket adapter may add those facilities outside this core without widening
the verifier's ordinary-input contract.

## Example

```rust
use axcbpf::{Instruction, Program, opcode};

let filter = Program::verify(&[
    Instruction::statement(opcode::LD_B_ABS, 0),
    Instruction::jump(opcode::JMP_JEQ_K, 0x45, 0, 1),
    Instruction::statement(opcode::RET_K, 1),
    Instruction::statement(opcode::RET_K, 0),
])?;

assert_eq!(filter.evaluate(&[0x45][..]), 1);
# Ok::<(), axcbpf::VerifyError>(())
```

See `CHANGELOG.md` for the public 0.1 contract.
