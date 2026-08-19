//! Linux classic-socket-filter ancillary metadata.
//!
//! Linux encodes these values as negative offsets in the `k` field of an
//! absolute load.  They are not packet addresses: treating an unknown
//! negative value as an ordinary byte offset would both diverge from Linux's
//! verifier and risk an out-of-bounds read.  Keep the wire constants and the
//! typed value provider together so the interpreter and x86 translator share
//! one semantic vocabulary.

/// Linux's base for classic-BPF socket-filter ancillary loads.
pub const SKF_AD_OFF: u32 = 0xffff_f000;
/// Protocol (host-order `skb->protocol`).
pub const SKF_AD_PROTOCOL: u32 = 0;
/// Packet type (`skb->pkt_type`).
pub const SKF_AD_PKTTYPE: u32 = 4;
/// Input interface index.
pub const SKF_AD_IFINDEX: u32 = 8;
/// Netlink attribute lookup (not implemented by the packet provider).
pub const SKF_AD_NLATTR: u32 = 12;
/// Nested netlink attribute lookup (not implemented by the packet provider).
pub const SKF_AD_NLATTR_NEST: u32 = 16;
/// Packet mark (`skb->mark`).
pub const SKF_AD_MARK: u32 = 20;
/// Receive queue mapping (`skb->queue_mapping`).
pub const SKF_AD_QUEUE: u32 = 24;
/// Link hardware type (`skb->dev->type`, not currently exposed by the packet
/// filter ABI).
pub const SKF_AD_HATYPE: u32 = 28;
/// Receive hash (not currently exposed by the packet filter ABI).
pub const SKF_AD_RXHASH: u32 = 32;
/// Current CPU (not currently exposed by the packet filter ABI).
pub const SKF_AD_CPU: u32 = 36;
/// The special A ^= X extension, which is not a load.
pub const SKF_AD_ALU_XOR_X: u32 = 40;
/// VLAN tag control information.
pub const SKF_AD_VLAN_TAG: u32 = 44;
/// Whether a VLAN tag is present.
pub const SKF_AD_VLAN_TAG_PRESENT: u32 = 48;
/// Packet payload offset (not currently exposed by the packet filter ABI).
pub const SKF_AD_PAY_OFFSET: u32 = 52;
/// Per-evaluation random value (not currently exposed by the packet filter
/// ABI).
pub const SKF_AD_RANDOM: u32 = 56;
/// VLAN protocol identifier.
pub const SKF_AD_VLAN_TPID: u32 = 60;
/// Size of the Linux ancillary offset window.
pub const SKF_AD_MAX: u32 = 64;

/// Typed ancillary values supported by the packet metadata provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Ancillary {
    /// Host-order link protocol.
    Protocol,
    /// Linux packet type value.
    Pkttype,
    /// Input interface index.
    Ifindex,
    /// Packet mark.
    Mark,
    /// Receive queue mapping.
    Queue,
    /// VLAN tag control information.
    VlanTag,
    /// VLAN tag presence, normalized to zero or one.
    VlanTagPresent,
    /// VLAN protocol identifier in host byte order.
    VlanTpid,
}

impl Ancillary {
    /// Returns the Linux offset within the ancillary window.
    pub const fn offset(self) -> u32 {
        match self {
            Self::Protocol => SKF_AD_PROTOCOL,
            Self::Pkttype => SKF_AD_PKTTYPE,
            Self::Ifindex => SKF_AD_IFINDEX,
            Self::Mark => SKF_AD_MARK,
            Self::Queue => SKF_AD_QUEUE,
            Self::VlanTag => SKF_AD_VLAN_TAG,
            Self::VlanTagPresent => SKF_AD_VLAN_TAG_PRESENT,
            Self::VlanTpid => SKF_AD_VLAN_TPID,
        }
    }

    /// Returns the aligned `PacketMetadata` field offset.
    pub const fn metadata_offset(self) -> usize {
        match self {
            Self::Protocol => 0,
            Self::Ifindex => 4,
            Self::Pkttype => 8,
            Self::Mark => 12,
            Self::Queue => 16,
            Self::VlanTag => 20,
            Self::VlanTagPresent => 24,
            Self::VlanTpid => 28,
        }
    }

    /// Returns the encoded negative `k` field used by Linux cBPF.
    pub const fn encoded_offset(self) -> u32 {
        SKF_AD_OFF.wrapping_add(self.offset())
    }

    /// Returns the value supplied by packet metadata.
    pub const fn read(self, metadata: PacketMetadata) -> u32 {
        match self {
            Self::Protocol => metadata.protocol,
            Self::Pkttype => metadata.pkttype,
            Self::Ifindex => metadata.ifindex,
            Self::Mark => metadata.mark,
            Self::Queue => metadata.queue,
            Self::VlanTag => metadata.vlan_tag,
            Self::VlanTagPresent => metadata.vlan_tag_present,
            Self::VlanTpid => metadata.vlan_tpid,
        }
    }
}

/// Packet metadata required by the supported Linux socket-filter extensions.
///
/// All fields are normalized to host-order `u32` values.  This deliberately
/// keeps the hot ancillary lowering to one aligned load and avoids packing
/// fields into a byte-level pseudo-packet.  The provider, rather than the
/// cBPF interpreter/JIT, owns Linux-specific defaults and conversions.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketMetadata {
    /// Host-order link protocol (`skb->protocol`).
    pub protocol: u32,
    /// Input interface index (`skb->dev->ifindex`), or zero when absent.
    pub ifindex: u32,
    /// Linux packet type (`skb->pkt_type`).
    pub pkttype: u32,
    /// Packet mark (`skb->mark`).
    pub mark: u32,
    /// Receive queue mapping (`skb->queue_mapping`).
    pub queue: u32,
    /// VLAN tag control information, zero when no tag is present.
    pub vlan_tag: u32,
    /// VLAN tag presence, normalized to zero or one.
    pub vlan_tag_present: u32,
    /// Host-order VLAN protocol identifier, zero when unavailable.
    pub vlan_tpid: u32,
}

impl PacketMetadata {
    /// Creates normalized packet metadata.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        protocol: u16,
        ifindex: u32,
        pkttype: u32,
        mark: u32,
        queue: u16,
        vlan_tag: u16,
        vlan_tag_present: bool,
        vlan_tpid: u16,
    ) -> Self {
        Self {
            protocol: protocol as u32,
            ifindex,
            // Linux masks `skb->pkt_type` with PKT_TYPE_MAX before exposing
            // the value to classic BPF (and shifts only on big-endian
            // bitfields).  The normalized provider value is host-order.
            pkttype: pkttype & 0x7,
            mark,
            queue: queue as u32,
            vlan_tag: vlan_tag as u32,
            vlan_tag_present: vlan_tag_present as u32,
            vlan_tpid: vlan_tpid as u32,
        }
    }

    /// Returns one typed ancillary value.
    pub const fn ancillary(self, field: Ancillary) -> u32 {
        field.read(self)
    }
}

/// A borrowed packet and its immutable metadata provider.
///
/// The byte slice remains the only source for ordinary packet offsets.  The
/// metadata path is separate, so an ancillary load can never turn into a
/// packet read with a wrapped or negative offset.
pub struct PacketInput<'a> {
    bytes: &'a [u8],
    metadata: PacketMetadata,
}

impl<'a> PacketInput<'a> {
    /// Creates a packet input with normalized metadata.
    pub const fn new(bytes: &'a [u8], metadata: PacketMetadata) -> Self {
        Self { bytes, metadata }
    }

    /// Returns the packet bytes.
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Returns the immutable metadata snapshot.
    pub const fn metadata(&self) -> PacketMetadata {
        self.metadata
    }
}

/// Provider of immutable packet metadata for a classic-BPF input.
pub trait PacketMetadataProvider {
    /// Returns the metadata snapshot for this packet evaluation.
    fn packet_metadata(&self) -> PacketMetadata;
}

impl PacketMetadataProvider for PacketInput<'_> {
    fn packet_metadata(&self) -> PacketMetadata {
        self.metadata
    }
}

/// Native-call context used by the packet-aware x86 translator.
///
/// The generated function still has the existing two-argument ABI.  For the
/// packet-aware profile its first argument points at this context, while the
/// second argument is ignored after the prologue loads `len` from the typed
/// context.  Keeping the data pointer and packet length in the context lets
/// the JIT read the original packet without copying it and makes metadata
/// loads ordinary fixed-offset, aligned loads from the same cache-hot
/// snapshot.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PacketInputContext {
    /// Original packet data pointer.
    pub data: *const u8,
    /// Original packet length.
    pub len: u32,
    /// Reserved ABI word; must be zero for now.
    pub reserved: u32,
    /// Immutable packet metadata.
    pub metadata: PacketMetadata,
}

impl PacketInputContext {
    /// Creates a context whose borrow must outlive the native call.
    pub fn new(bytes: &[u8], metadata: PacketMetadata) -> Self {
        Self {
            data: bytes.as_ptr(),
            len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            reserved: 0,
            metadata,
        }
    }

    /// Byte offset of the original packet pointer in the context.
    pub const DATA_OFFSET: usize = core::mem::offset_of!(Self, data);
    /// Byte offset of the packet length in the context.
    pub const LEN_OFFSET: usize = core::mem::offset_of!(Self, len);
    /// Byte offset of the metadata snapshot in the context.
    pub const METADATA_OFFSET: usize = core::mem::offset_of!(Self, metadata);
}

/// Resolves one Linux `SKF_AD_*` encoded `k` field to a supported typed
/// ancillary.  Unknown negative offsets deliberately return `None` so callers
/// can reject them instead of treating them as packet addresses.
pub const fn ancillary_from_offset(offset: u32) -> Option<Ancillary> {
    match offset {
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_PROTOCOL) => Some(Ancillary::Protocol),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_PKTTYPE) => Some(Ancillary::Pkttype),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_IFINDEX) => Some(Ancillary::Ifindex),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_MARK) => Some(Ancillary::Mark),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_QUEUE) => Some(Ancillary::Queue),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_VLAN_TAG) => Some(Ancillary::VlanTag),
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_VLAN_TAG_PRESENT) => {
            Some(Ancillary::VlanTagPresent)
        }
        value if value == SKF_AD_OFF.wrapping_add(SKF_AD_VLAN_TPID) => Some(Ancillary::VlanTpid),
        _ => None,
    }
}

/// Returns whether a `k` field is in Linux's negative ancillary window.
pub const fn is_ancillary_offset(offset: u32) -> bool {
    offset.wrapping_sub(SKF_AD_OFF) < SKF_AD_MAX
}
