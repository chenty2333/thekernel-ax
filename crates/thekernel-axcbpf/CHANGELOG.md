# Changelog

## 0.1.0

- Add the standard eight-byte classic-BPF instruction representation.
- Add fallible verification and immutable ownership for programs of at most
  4096 instructions, including zero-copy adoption of an existing instruction
  vector after validation.
- Validate the ordinary classic-BPF opcode set, forward branch geometry,
  immediate arithmetic constraints, sixteen scratch words, and all-path
  scratch initialization.
- Add an allocation-free A/X/M interpreter with bounded execution, full
  ordinary byte/halfword/word absolute and indirect loads, MSH, and MOD.
- Add an input trait that leaves domain-specific byte order and range policy to
  adapters, plus a network-byte-order byte-slice implementation.
- Reject unsupported ancillary extensions explicitly and keep Linux seccomp,
  socket, task, signal, and errno policy outside the crate.
