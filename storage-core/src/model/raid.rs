//! RAID-related model semantics.

use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use super::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LvmRaid0Variant {
    Raid0,
    Raid0Meta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParityRotation {
    LeftSymmetric,
    LeftAsymmetric,
    RightSymmetric,
    RightAsymmetric,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LvmRaid5Layout {
    Rotating(ParityRotation),
    ParityN,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LvmRaid6Layout {
    ZeroRestart,
    NRestart,
    NContinue,
    /// raid6_n_6
    DedicatedParity,
    /// raid6_{ls,la,rs,ra}_6
    Raid5Compatible(ParityRotation),
}

/// Number of complete data copies in a mirrored RAID layout.
///
/// `2` means one original-sized data image plus one mirror. Values below `2`
/// are not RAID replication and must be represented by a non-RAID layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CopyCount(NonZeroU16);

impl CopyCount {
    /// Creates a copy count for mirrored RAID layouts.
    pub fn new(value: u16) -> Option<Self> {
        (value >= 2)
            .then(|| NonZeroU16::new(value).map(Self))
            .flatten()
    }

    /// Returns the number of complete data copies.
    pub fn get(self) -> u16 {
        self.0.get()
    }
}

/// Number of data-bearing stripes, excluding parity and mirror copies.
///
/// For RAID5 this is `devices - 1`, for RAID6 it is `devices - 2`, and for
/// RAID10 it is the number of striped mirror groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DataStripeCount(NonZeroU16);

impl DataStripeCount {
    /// Creates a data stripe count.
    pub fn new(value: u16) -> Option<Self> {
        NonZeroU16::new(value).map(Self)
    }

    /// Returns the number of data-bearing stripes.
    pub fn get(self) -> u16 {
        self.0.get()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LvmRaid {
    Raid0 {
        variant: LvmRaid0Variant,
        data_width: DataStripeCount,
        stripe_size: Bytes,
    },
    Raid1 {
        copies: CopyCount,
    },
    Raid4 {
        data_width: DataStripeCount,
        stripe_size: Bytes,
    },
    Raid5 {
        layout: LvmRaid5Layout,
        data_width: DataStripeCount,
        stripe_size: Bytes,
    },
    Raid6 {
        layout: LvmRaid6Layout,
        data_width: DataStripeCount,
        stripe_size: Bytes,
    },
    Raid10 {
        copies: CopyCount,
        data_width: DataStripeCount,
        stripe_size: Bytes,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdRaid0Layout {
    Original,
    Alternate,
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdParityLayout {
    Rotating(ParityRotation),
    ParityFirst,
    ParityLast,
    DdfZeroRestart,
    DdfNRestart,
    DdfNContinue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdRaid6DedicatedQ {
    LeftSymmetric6,
    RightSymmetric6,
    LeftAsymmetric6,
    RightAsymmetric6,
    ParityFirst6,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdRaid6Layout {
    Standard(MdParityLayout),
    DedicatedQ(MdRaid6DedicatedQ),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdRaid10Layout {
    pub mode: MdRaid10Mode,
    pub copies: CopyCount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdRaid10Mode {
    Near,
    Offset,
    Far,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdRaid {
    Raid0 {
        layout: MdRaid0Layout,
        chunk_size: Bytes,
    },
    Raid1,
    Raid4 {
        chunk_size: Bytes,
    },
    Raid5 {
        layout: MdParityLayout,
        chunk_size: Bytes,
    },
    Raid6 {
        layout: MdRaid6Layout,
        chunk_size: Bytes,
    },
    Raid10 {
        layout: MdRaid10Layout,
        chunk_size: Bytes,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdPersonality {
    Raid(MdRaid),
    Linear { rounding: Option<Bytes> },
    Multipath,
    Faulty,
    Container,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdMetadata {
    None,
    Native(MdNativeMetadata),
    External(MdExternalMetadata),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdNativeMetadata {
    V0_90,
    V1_0,
    V1_1,
    V1_2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MdExternalMetadata {
    Ddf,
    Imsm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdArray {
    pub personality: MdPersonality,
    pub metadata: MdMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Copy)]
#[serde(rename_all = "lowercase")]
pub enum BtrfsProfile {
    Single,
    Dup,
    Raid0,
    Raid1,
    Raid1C3,
    Raid1C4,
    Raid10,
    Raid5,
    Raid6,
}
