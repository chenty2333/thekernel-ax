# Changelog

## 0.1.0

- Add a fixed-capacity, allocation-free PMU session contract whose counters
  remain stopped until an explicit start.
- Add typed cycle, instruction, D-TLB-miss, and I-TLB-miss events with explicit
  backend negotiation failures.
- Add default-off, saturating software diagnostics for ASID fast-switch paths.
