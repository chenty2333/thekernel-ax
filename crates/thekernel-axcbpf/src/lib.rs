#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod instruction;
mod program;

pub use instruction::{Instruction, opcode};
pub use program::{Input, LoadWidth, MAX_INSTRUCTIONS, Program, SCRATCH_WORDS, VerifyError};
