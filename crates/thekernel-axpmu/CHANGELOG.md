# Changelog

## 0.1.0

- Add a fixed-capacity, allocation-free PMU session contract whose counters
  remain stopped until an explicit start.
- Add typed cycle, instruction, D-TLB-miss, and I-TLB-miss events with RISC-V
  SBI encodings and explicit backend negotiation failures.
- Add an opt-in RISC-V SBI backend with injected hardware-CSR reads and an
  explicit unsupported LoongArch backend pending a verified platform adapter.
- Add default-off, saturating software diagnostics for ASID fast-switch paths.
