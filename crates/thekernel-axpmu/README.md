# thekernel-axpmu

`thekernel-axpmu` is a `no_std`, allocation-free mechanism crate for bounded,
explicitly enabled performance monitoring. It owns neither benchmark policy
nor Linux `perf_event_open(2)` semantics.

## Contract

A `Session<B, N>` accepts at most `N` distinct typed events. Opening a session
may reserve and configure counters, but it does not start them. Callers must
explicitly call `start`; `stop`, `snapshot`, and cleanup preserve backend
failures instead of manufacturing samples. Snapshots use fixed arrays and are
sequential unless a future backend advertises a stronger contract.

`Event` includes CPU cycles, completed instructions, data-TLB misses, and
instruction-TLB misses. A capability's event mask describes events that can be
requested, not a promise that a particular platform has a matching counter.
`Backend::configure` remains the authoritative negotiation point.

The optional `riscv-sbi` backend uses the SBI PMU extension to probe,
configure, start, and stop counters. SBI reports a hardware counter's CSR
number at run time, while Rust inline assembly requires a compile-time CSR
operand. The platform therefore injects `RiscvHardwareCounterReader`; no CSR
number is guessed by this crate. Firmware counters are read through SBI.

LoongArch PMCFG/PMCNT register details are intentionally absent until a
verified platform implementation supplies them. `LoongArchPmu` reports
`Error::Unsupported` rather than exposing a fake counter.

`SoftwareDiagnostics` is a separate default-off mechanism for already
classified hot-path facts such as an ASID switch avoiding a full TLB flush. It
does not decide whether a switch was safe and does not classify policy reasons.
The disabled path performs one relaxed atomic load; the enabled increment is
saturating and allocation-free.

## Example

```rust
use axpmu::{Event, Session};

# use axpmu::{Backend, Capabilities, CounterSource, Error, EventMask};
# #[derive(Clone, Copy)] struct Counter;
# struct Demo;
# impl Backend for Demo {
#     type Counter = Counter;
#     fn capabilities(&self) -> Capabilities {
#         Capabilities::new(CounterSource::Platform, 2, EventMask::ALL, false)
#     }
#     fn configure(&mut self, _: Event) -> Result<Counter, Error> { Ok(Counter) }
#     fn start(&mut self, _: Counter) -> Result<(), Error> { Ok(()) }
#     fn read(&mut self, _: Counter) -> Result<u64, Error> { Ok(7) }
#     fn stop(&mut self, _: Counter) -> Result<(), Error> { Ok(()) }
#     fn release(&mut self, _: Counter) -> Result<(), Error> { Ok(()) }
# }
# let mut backend = Demo;
let mut session = Session::<_, 2>::open(
    &mut backend,
    &[Event::CpuCycles, Event::Instructions],
)?;
session.start()?;
let snapshot = session.snapshot()?;
assert_eq!(snapshot.len(), 2);
session.stop()?;
# Ok::<(), Error>(())
```

See `CHANGELOG.md` for the public 0.1 contract.
