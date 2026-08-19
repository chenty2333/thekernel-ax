#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

mod ancillary;
mod instruction;
mod program;
mod translate;
mod translation_validate;

pub use ancillary::{
    Ancillary, PacketInput, PacketInputContext, PacketMetadata, PacketMetadataProvider,
    SKF_AD_ALU_XOR_X, SKF_AD_CPU, SKF_AD_HATYPE, SKF_AD_IFINDEX, SKF_AD_MARK, SKF_AD_MAX,
    SKF_AD_NLATTR, SKF_AD_NLATTR_NEST, SKF_AD_OFF, SKF_AD_PAY_OFFSET, SKF_AD_PKTTYPE,
    SKF_AD_PROTOCOL, SKF_AD_QUEUE, SKF_AD_RANDOM, SKF_AD_RXHASH, SKF_AD_VLAN_TAG,
    SKF_AD_VLAN_TAG_PRESENT, SKF_AD_VLAN_TPID, ancillary_from_offset, is_ancillary_offset,
};
pub use instruction::{Instruction, opcode};
pub use program::{Input, LoadWidth, MAX_INSTRUCTIONS, Program, SCRATCH_WORDS, VerifyError};
pub use translate::{
    CodeImage, ExternalCall, ImageValidationError, InputProfile, InstructionMap,
    MAX_CODE_IMAGE_BYTES, NativeWordInput, Relocation, RelocationKind, TranslationError,
    TranslationValidator, validate_translation,
};
pub use translation_validate::validate_translation_bytes;
