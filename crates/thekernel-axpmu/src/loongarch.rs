//! Platform-injected LoongArch PMCFG/PMCNT backend.

use crate::{Backend, Capabilities, CounterSource, Error, Event, EventMask};

/// Architectural maximum number of LoongArch performance monitors.
pub const LOONGARCH_MAX_COUNTERS: usize = 32;

/// Platform contract for verified LoongArch PMCFG/PMCNT access.
///
/// The platform adapter owns all event numbers, CSR instruction selection,
/// privilege-level counting policy, and CPUCFG probing. The generic backend
/// never interprets an event encoding or constructs a raw PMCFG value.
///
/// A `write_pmcfg` call with `Some(encoding)` must program that event. With
/// `enabled == false`, it must leave every PLV counting-enable bit clear. With
/// `enabled == true`, it must apply the adapter's explicit, documented PLV
/// policy. This crate does not consume overflow interrupts, so the adapter must
/// leave PMI disabled. `None` must disable the monitor and clear its mapping.
/// Reserved PMCFG bits must always be written as zero.
///
/// Failed writes must not leave the monitor enabled; they must either preserve
/// the prior stopped mapping or clear it. Every index accepted by these methods
/// must be below the verified count returned by [`Self::counter_count`].
pub trait LoongArchPlatform {
    /// Opaque, platform-verified event encoding.
    type EventEncoding: Copy;

    /// Returns the CPUCFG-derived number of implemented monitors.
    fn counter_count(&self) -> usize;

    /// Returns typed events for which this adapter has a verified encoding.
    fn requestable_events(&self) -> EventMask;

    /// Maps a typed event to a processor-appropriate event encoding.
    fn event_encoding(&self, event: Event) -> Result<Self::EventEncoding, Error>;

    /// Writes one PMCFG mapping and counting-enable state.
    fn write_pmcfg(
        &mut self,
        counter: usize,
        encoding: Option<Self::EventEncoding>,
        enabled: bool,
    ) -> Result<(), Error>;

    /// Writes one PMCNT value, used to establish the zero baseline.
    fn write_pmcnt(&mut self, counter: usize, value: u64) -> Result<(), Error>;

    /// Reads one PMCNT value.
    fn read_pmcnt(&mut self, counter: usize) -> Result<u64, Error>;
}

/// Opaque generation-tagged LoongArch counter handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoongArchCounter<E: Copy> {
    index: usize,
    generation: u64,
    encoding: E,
}

impl<E: Copy> LoongArchCounter<E> {
    /// Returns the platform counter index for diagnostics.
    pub const fn index(self) -> usize {
        self.index
    }
}

/// Fixed-capacity LoongArch PMCFG/PMCNT backend.
///
/// `MAX_COUNTERS` is a consumer-selected storage bound. The backend exposes the
/// minimum of that bound, the platform's CPUCFG-derived count, and the
/// architectural maximum of 32. It allocates no memory.
pub struct LoongArchPlatformPmu<P: LoongArchPlatform, const MAX_COUNTERS: usize> {
    platform: P,
    counter_count: usize,
    claimed: [bool; MAX_COUNTERS],
    running: [bool; MAX_COUNTERS],
    generations: [u64; MAX_COUNTERS],
}

impl<P: LoongArchPlatform, const MAX_COUNTERS: usize> LoongArchPlatformPmu<P, MAX_COUNTERS> {
    /// Creates a stopped backend from a platform adapter.
    ///
    /// Construction reads capability metadata only and performs no PMCFG or
    /// PMCNT write.
    pub fn new(platform: P) -> Self {
        let counter_count = platform
            .counter_count()
            .min(MAX_COUNTERS)
            .min(LOONGARCH_MAX_COUNTERS);
        Self {
            platform,
            counter_count,
            claimed: [false; MAX_COUNTERS],
            running: [false; MAX_COUNTERS],
            generations: [0; MAX_COUNTERS],
        }
    }

    /// Returns the bounded counter count exposed by this backend.
    pub const fn counter_count(&self) -> usize {
        self.counter_count
    }

    /// Returns shared access to the platform adapter.
    pub const fn platform(&self) -> &P {
        &self.platform
    }

    /// Returns mutable access to the platform adapter.
    ///
    /// Callers must not modify monitors currently claimed by this backend.
    pub fn platform_mut(&mut self) -> &mut P {
        &mut self.platform
    }

    /// Consumes an idle backend and returns the platform adapter.
    ///
    /// Returns [`Error::InvalidRequest`] while any counter remains claimed.
    pub fn try_into_platform(self) -> Result<P, Error> {
        if self.claimed[..self.counter_count].contains(&true) {
            Err(Error::InvalidRequest)
        } else {
            Ok(self.platform)
        }
    }

    fn validate_known(&self, counter: LoongArchCounter<P::EventEncoding>) -> Result<(), Error> {
        if counter.index >= self.counter_count
            || self.generations[counter.index] != counter.generation
        {
            Err(Error::InvalidRequest)
        } else {
            Ok(())
        }
    }

    fn validate_claimed(&self, counter: LoongArchCounter<P::EventEncoding>) -> Result<(), Error> {
        self.validate_known(counter)?;
        if self.claimed[counter.index] {
            Ok(())
        } else {
            Err(Error::InvalidRequest)
        }
    }
}

impl<P: LoongArchPlatform, const MAX_COUNTERS: usize> Backend
    for LoongArchPlatformPmu<P, MAX_COUNTERS>
{
    type Counter = LoongArchCounter<P::EventEncoding>;

    fn capabilities(&self) -> Capabilities {
        if self.counter_count == 0 {
            Capabilities::unsupported(CounterSource::LoongArchCsr)
        } else {
            Capabilities::new(
                CounterSource::LoongArchCsr,
                self.counter_count,
                self.platform.requestable_events(),
                false,
            )
        }
    }

    fn configure(&mut self, event: Event) -> Result<Self::Counter, Error> {
        if !self.platform.requestable_events().contains(event) {
            return Err(Error::Unsupported);
        }
        let encoding = self.platform.event_encoding(event)?;

        for index in 0..self.counter_count {
            if self.claimed[index] {
                continue;
            }
            let generation = match self.generations[index].checked_add(1) {
                Some(generation) => generation,
                None => continue,
            };
            match self.platform.write_pmcfg(index, Some(encoding), false) {
                Ok(()) => {
                    self.generations[index] = generation;
                    self.claimed[index] = true;
                    self.running[index] = false;
                    return Ok(LoongArchCounter {
                        index,
                        generation,
                        encoding,
                    });
                }
                Err(Error::Unsupported | Error::NoCounter) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::NoCounter)
    }

    fn start(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate_claimed(counter)?;
        if self.running[counter.index] {
            return Err(Error::AlreadyRunning);
        }
        self.platform.write_pmcnt(counter.index, 0)?;
        self.platform
            .write_pmcfg(counter.index, Some(counter.encoding), true)?;
        self.running[counter.index] = true;
        Ok(())
    }

    fn read(&mut self, counter: Self::Counter) -> Result<u64, Error> {
        self.validate_claimed(counter)?;
        self.platform.read_pmcnt(counter.index)
    }

    fn stop(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate_known(counter)?;
        if !self.claimed[counter.index] || !self.running[counter.index] {
            return Ok(());
        }
        self.platform
            .write_pmcfg(counter.index, Some(counter.encoding), false)?;
        self.running[counter.index] = false;
        Ok(())
    }

    fn release(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate_known(counter)?;
        if !self.claimed[counter.index] {
            return Ok(());
        }
        self.platform.write_pmcfg(counter.index, None, false)?;
        self.running[counter.index] = false;
        self.claimed[counter.index] = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, Snapshot};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Encoding(u16);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Config {
        encoding: Option<Encoding>,
        enabled: bool,
    }

    struct FakePlatform {
        count: usize,
        configs: [Config; 4],
        values: [u64; 4],
        writes: usize,
    }

    impl FakePlatform {
        fn new(count: usize) -> Self {
            Self {
                count,
                configs: [Config {
                    encoding: None,
                    enabled: false,
                }; 4],
                values: [0; 4],
                writes: 0,
            }
        }
    }

    impl LoongArchPlatform for FakePlatform {
        type EventEncoding = Encoding;

        fn counter_count(&self) -> usize {
            self.count
        }

        fn requestable_events(&self) -> EventMask {
            EventMask::from_event(Event::CpuCycles)
                .union(EventMask::from_event(Event::Instructions))
        }

        fn event_encoding(&self, event: Event) -> Result<Self::EventEncoding, Error> {
            match event {
                Event::CpuCycles => Ok(Encoding(0x10)),
                Event::Instructions => Ok(Encoding(0x11)),
                _ => Err(Error::Unsupported),
            }
        }

        fn write_pmcfg(
            &mut self,
            counter: usize,
            encoding: Option<Self::EventEncoding>,
            enabled: bool,
        ) -> Result<(), Error> {
            let config = self.configs.get_mut(counter).ok_or(Error::InvalidRequest)?;
            *config = Config { encoding, enabled };
            self.writes += 1;
            Ok(())
        }

        fn write_pmcnt(&mut self, counter: usize, value: u64) -> Result<(), Error> {
            *self.values.get_mut(counter).ok_or(Error::InvalidRequest)? = value;
            self.writes += 1;
            Ok(())
        }

        fn read_pmcnt(&mut self, counter: usize) -> Result<u64, Error> {
            self.values
                .get(counter)
                .copied()
                .ok_or(Error::InvalidRequest)
        }
    }

    fn assert_snapshot(snapshot: Snapshot<2>) {
        assert_eq!(snapshot.events(), [Event::CpuCycles, Event::Instructions]);
        assert_eq!(snapshot.values(), [0, 0]);
        assert!(!snapshot.is_consistent());
    }

    #[test]
    fn platform_backend_runs_a_bounded_session_without_raw_event_assumptions() {
        let platform = FakePlatform::new(4);
        let mut backend = LoongArchPlatformPmu::<_, 2>::new(platform);
        assert_eq!(backend.counter_count(), 2);

        let events = [Event::CpuCycles, Event::Instructions];
        let mut session = Session::<_, 2>::open(&mut backend, &events).unwrap();
        assert!(!session.is_running());
        session.start().unwrap();
        assert_snapshot(session.snapshot().unwrap());
        session.stop().unwrap();
        session.close().unwrap();

        let platform = backend.try_into_platform().unwrap();
        assert_eq!(
            platform.configs[..2],
            [
                Config {
                    encoding: None,
                    enabled: false
                },
                Config {
                    encoding: None,
                    enabled: false
                }
            ]
        );
        assert!(platform.writes >= 8);
    }

    #[test]
    fn release_is_idempotent_and_generation_rejects_a_stale_handle() {
        let mut backend = LoongArchPlatformPmu::<_, 1>::new(FakePlatform::new(1));
        let first = backend.configure(Event::CpuCycles).unwrap();
        backend.release(first).unwrap();
        backend.release(first).unwrap();

        let second = backend.configure(Event::Instructions).unwrap();
        assert_eq!(backend.start(first), Err(Error::InvalidRequest));
        backend.start(second).unwrap();
        backend.stop(second).unwrap();
        backend.stop(second).unwrap();
        backend.release(second).unwrap();
    }

    #[test]
    fn unsupported_events_and_zero_capacity_fail_without_register_writes() {
        let mut zero = LoongArchPlatformPmu::<_, 0>::new(FakePlatform::new(4));
        assert!(!zero.capabilities().is_available());
        assert_eq!(zero.configure(Event::CpuCycles), Err(Error::NoCounter));
        assert_eq!(zero.platform().writes, 0);

        let mut backend = LoongArchPlatformPmu::<_, 2>::new(FakePlatform::new(4));
        assert_eq!(
            backend.configure(Event::DataTlbReadMisses),
            Err(Error::Unsupported)
        );
        assert_eq!(backend.platform().writes, 0);
    }
}
