# thekernel-axcbpf

`thekernel-axcbpf` is a `no_std`, unsafe-free classic-BPF verifier and
interpreter for operating-system mechanism layers. It accepts the ordinary
classic instruction set, including packet-style absolute and indirect loads,
sixteen scratch words, forward jumps, 32-bit arithmetic, and the common Linux
socket-filter ancillary fields (protocol, packet type, interface index, mark,
queue, and VLAN metadata).

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

Linux `SKF_AD_*` loads are resolved through a typed `PacketMetadata` provider;
unknown or unsupported negative offsets are rejected rather than treated as
packet addresses. `PacketInput` is the allocation-free interpreter adapter.
The packet-aware x86 profile uses a `PacketInputContext`, retaining the
two-argument native entry ABI while loading the original packet pointer,
length, and aligned metadata fields from one immutable context snapshot.
Ancillary values never participate in packet bounds arithmetic, so a metadata
load cannot read outside the packet.

`Program::translate` emits a real immutable SysV x86_64 function with ABI
`extern "C" fn(*const u8, u32) -> u32`. The image contains no helper calls or
address-bearing relocations: input bounds, indirect-offset overflow, arithmetic,
branches, and failure-to-zero paths are emitted directly. A publisher may copy
`CodeImage::bytes` into a W^X executable mapping; `entry()` is zero and
`page_aligned_size_upper_bound()` gives the exact mapping upper bound. Use
`InputProfile::PacketContextBigEndian` for a packet filter that uses ancillary
loads; its first argument points to `PacketInputContext` rather than directly
to packet bytes.

Use `Program::translate_with_profile(InputProfile::NativeAlignedWords)` for a
seccomp-style native-endian aligned-word input. That profile rejects byte and
halfword source loads, rejects unaligned absolute word offsets, and checks
indirect word alignment at runtime. `NativeWordInput` is the corresponding
safe reference-model adapter. Both the interpreter and JIT check indirect word
alignment using the logical offset before adding the data pointer.
`NativeWordInput::new` intentionally keeps that logical rule independent from
an incidental Rust slice address; adapters with a four-byte-aligned
snapshot-base ABI can use `NativeWordInput::new_aligned` at their boundary.
`TranslationValidator` independently decodes the restricted native instruction
subset, checks the source semantic trace, direct targets, prologue/epilogue,
and the empty relocation set; it does not make pages executable.

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
