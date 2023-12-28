use super::{BLOCK_SIZE, SECTOR_SIZE};

use core::{
    iter::Step,
    ops::{Add, Sub},
};
use pod::Pod;
use static_assertions::const_assert;

/// The block ID used in the FS.
pub type Bid = BlockId<BLOCK_SIZE>;

/// The sector ID used in the device.
pub type Sid = BlockId<SECTOR_SIZE>;

const_assert!(BLOCK_SIZE >= SECTOR_SIZE);

impl From<Bid> for Sid {
    fn from(bid: Bid) -> Self {
        Self::new(bid.to_raw() * (BLOCK_SIZE / SECTOR_SIZE) as u64)
    }
}

/// The ID of a block consisting of `N_BYTES` bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Pod)]
pub struct BlockId<const N_BYTES: usize>(u64);

impl<const N_BYTES: usize> BlockId<N_BYTES> {
    /// Constructs an id from a raw id.
    pub const fn new(raw_id: u64) -> Self {
        Self(raw_id)
    }

    /// Constructs an id from a byte offset.
    pub const fn from_offset(offset: usize) -> Self {
        Self((offset / N_BYTES) as _)
    }

    /// Converts to a byte offset.
    pub fn to_offset(self) -> usize {
        (self.0 as usize) * N_BYTES
    }

    /// Converts to raw id.
    pub fn to_raw(self) -> u64 {
        self.0
    }
}

impl<const N_BYTES: usize> Add<u64> for BlockId<N_BYTES> {
    type Output = Self;

    fn add(self, other: u64) -> Self::Output {
        Self(self.0 + other)
    }
}

impl<const N_BYTES: usize> Sub<u64> for BlockId<N_BYTES> {
    type Output = Self;

    fn sub(self, other: u64) -> Self::Output {
        Self(self.0 - other)
    }
}

/// Implements the `Step` trait to iterate over `Range<Id>`.
impl<const N_BYTES: usize> Step for BlockId<N_BYTES> {
    fn steps_between(start: &Self, end: &Self) -> Option<usize> {
        u64::steps_between(&start.0, &end.0)
    }

    fn forward_checked(start: Self, count: usize) -> Option<Self> {
        u64::forward_checked(start.0, count).map(Self::new)
    }

    fn backward_checked(start: Self, count: usize) -> Option<Self> {
        u64::backward_checked(start.0, count).map(Self::new)
    }
}
