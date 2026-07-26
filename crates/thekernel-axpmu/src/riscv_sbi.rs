//! RISC-V SBI PMU backend.

use crate::{Backend, Capabilities, CounterSource, Error, Event, EventMask};

const CONFIG_CLEAR_VALUE: usize = 1 << 1;
const START_SET_INIT_VALUE: usize = 1;
const STOP_RESET_MAPPING: usize = 1;

/// Description of one directly readable RISC-V hardware counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiscvHardwareCounter {
    logical_index: usize,
    csr: u16,
    width: u8,
}

impl RiscvHardwareCounter {
    /// Returns the SBI logical counter index.
    pub const fn logical_index(self) -> usize {
        self.logical_index
    }

    /// Returns the supervisor-visible counter CSR reported by SBI.
    pub const fn csr(self) -> u16 {
        self.csr
    }

    /// Returns the counter width in bits.
    pub const fn width(self) -> u8 {
        self.width
    }
}

/// Platform-provided reader for a supervisor-visible hardware counter CSR.
///
/// SBI reports a CSR number dynamically, while inline assembly normally needs
/// a compile-time CSR operand. Implementations should match only CSRs their
/// platform adapter can read and return [`Error::ValueUnavailable`] for every
/// other value. This trait does not grant permission to guess a CSR mapping.
pub trait RiscvHardwareCounterReader {
    /// Reads one hardware counter described by SBI.
    fn read_hardware_counter(&mut self, counter: RiscvHardwareCounter) -> Result<u64, Error>;
}

/// Opaque configured RISC-V SBI counter handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiscvCounter {
    index: usize,
    generation: u64,
    firmware: bool,
    csr: u16,
    width: u8,
}

impl RiscvCounter {
    /// Returns the SBI logical counter index for diagnostics.
    pub const fn logical_index(self) -> usize {
        self.index
    }

    /// Returns whether this is an SBI firmware counter.
    pub const fn is_firmware(self) -> bool {
        self.firmware
    }
}

/// Bounded RISC-V backend using the SBI PMU extension.
///
/// Construction probes the extension but starts no counter. At most XLEN
/// logical counters are exposed so every allocation remains representable by
/// one SBI counter mask and one local machine word.
pub struct RiscvSbiPmu<R> {
    reader: R,
    counter_count: usize,
    claimed: usize,
    generations: [u64; usize::BITS as usize],
}

impl<R> RiscvSbiPmu<R> {
    /// Probes the calling hart's SBI PMU extension.
    ///
    /// This must run in an environment where an SBI `ecall` is valid.
    pub fn probe(reader: R) -> Self {
        let counter_count = if sbi_rt::probe_extension(sbi_rt::Pmu).is_available() {
            sbi_rt::pmu_num_counters().min(usize::BITS as usize)
        } else {
            0
        };
        Self {
            reader,
            counter_count,
            claimed: 0,
            generations: [0; usize::BITS as usize],
        }
    }

    /// Returns the platform reader owned by this backend.
    pub const fn reader(&self) -> &R {
        &self.reader
    }

    /// Returns mutable access to the platform reader.
    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consumes the backend and returns its platform reader.
    pub fn into_reader(self) -> R {
        self.reader
    }

    const fn available_mask(&self) -> usize {
        let valid = if self.counter_count == usize::BITS as usize {
            usize::MAX
        } else if self.counter_count == 0 {
            0
        } else {
            (1usize << self.counter_count) - 1
        };
        valid & !self.claimed
    }

    fn validate(&self, counter: RiscvCounter) -> Result<(), Error> {
        if counter.index >= self.counter_count
            || self.claimed & (1usize << counter.index) == 0
            || self.generations[counter.index] != counter.generation
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }

    fn counter_set(counter: RiscvCounter) -> (usize, usize) {
        (counter.index, 1)
    }

    fn map_sbi(result: sbi_rt::SbiRet) -> Result<usize, Error> {
        match result.error as isize {
            0 => Ok(result.value),
            -2 => Err(Error::Unsupported),
            -3 => Err(Error::InvalidRequest),
            -7 => Err(Error::AlreadyRunning),
            -8 => Err(Error::NotRunning),
            code => Err(Error::BackendFailure(code)),
        }
    }

    fn read_firmware(index: usize) -> Result<u64, Error> {
        #[cfg(target_pointer_width = "64")]
        {
            Self::map_sbi(sbi_rt::pmu_counter_fw_read(index)).map(|value| value as u64)
        }

        #[cfg(target_pointer_width = "32")]
        {
            let high_before = Self::map_sbi(sbi_rt::pmu_counter_fw_read_hi(index))? as u64;
            let mut low = Self::map_sbi(sbi_rt::pmu_counter_fw_read(index))? as u64;
            let high_after = Self::map_sbi(sbi_rt::pmu_counter_fw_read_hi(index))? as u64;
            if high_before != high_after {
                low = Self::map_sbi(sbi_rt::pmu_counter_fw_read(index))? as u64;
            }
            Ok((high_after << 32) | low)
        }
    }

    fn stop_raw(counter: RiscvCounter, flags: usize) -> Result<(), Error> {
        let (base, mask) = Self::counter_set(counter);
        match Self::map_sbi(sbi_rt::pmu_counter_stop(base, mask, flags)) {
            Ok(_) | Err(Error::NotRunning) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn reset_mapping(index: usize) {
        let _ = Self::map_sbi(sbi_rt::pmu_counter_stop(index, 1, STOP_RESET_MAPPING));
    }
}

impl<R: RiscvHardwareCounterReader> Backend for RiscvSbiPmu<R> {
    type Counter = RiscvCounter;

    fn capabilities(&self) -> Capabilities {
        if self.counter_count == 0 {
            Capabilities::unsupported(CounterSource::RiscvSbi)
        } else {
            Capabilities::new(
                CounterSource::RiscvSbi,
                self.counter_count,
                EventMask::ALL,
                false,
            )
        }
    }

    fn configure(&mut self, event: Event) -> Result<Self::Counter, Error> {
        let available = self.available_mask();
        if available == 0 {
            return Err(Error::NoCounter);
        }

        let encoding = event.riscv_sbi_encoding();
        let index = Self::map_sbi(sbi_rt::pmu_counter_config_matching(
            0,
            available,
            CONFIG_CLEAR_VALUE,
            encoding.event_idx(),
            encoding.event_data(),
        ))?;
        if index >= self.counter_count || available & (1usize << index) == 0 {
            Self::reset_mapping(index);
            return Err(Error::InvalidRequest);
        }

        let raw_info = match Self::map_sbi(sbi_rt::pmu_counter_get_info(index)) {
            Ok(info) => info,
            Err(error) => {
                Self::reset_mapping(index);
                return Err(error);
            }
        };

        let generation = match self.generations[index].checked_add(1) {
            Some(generation) => generation,
            None => {
                Self::reset_mapping(index);
                return Err(Error::NoCounter);
            }
        };
        let firmware = raw_info >> (usize::BITS - 1) != 0;
        let counter = RiscvCounter {
            index,
            generation,
            firmware,
            csr: (raw_info & 0xfff) as u16,
            width: (((raw_info >> 12) & 0x3f) + 1) as u8,
        };
        self.generations[index] = generation;
        self.claimed |= 1usize << index;
        Ok(counter)
    }

    fn start(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate(counter)?;
        let (base, mask) = Self::counter_set(counter);
        Self::map_sbi(sbi_rt::pmu_counter_start(
            base,
            mask,
            START_SET_INIT_VALUE,
            0,
        ))
        .map(|_| ())
    }

    fn read(&mut self, counter: Self::Counter) -> Result<u64, Error> {
        self.validate(counter)?;
        if counter.firmware {
            Self::read_firmware(counter.index)
        } else {
            self.reader.read_hardware_counter(RiscvHardwareCounter {
                logical_index: counter.index,
                csr: counter.csr,
                width: counter.width,
            })
        }
    }

    fn stop(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate(counter)?;
        Self::stop_raw(counter, 0)
    }

    fn release(&mut self, counter: Self::Counter) -> Result<(), Error> {
        self.validate(counter)?;
        Self::stop_raw(counter, STOP_RESET_MAPPING)?;
        self.claimed &= !(1usize << counter.index);
        Ok(())
    }
}
