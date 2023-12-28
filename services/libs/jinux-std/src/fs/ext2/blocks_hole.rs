use bitvec::prelude::BitVec;

/// A blocks hole descriptor implemented by the `BitVec`.
///
/// The true bit implies that the block is a hole, and conversely.
pub(super) struct BlocksHoleDesc(BitVec);

impl BlocksHoleDesc {
    /// Construct a new blocks hole descriptor.
    ///
    /// The `initial_len` usually is the number of blocks for a file.
    pub fn new(initial_len: u32) -> Self {
        let mut bit_vec = BitVec::with_capacity(initial_len as usize);
        bit_vec.resize(initial_len as usize, false);
        Self(bit_vec)
    }

    /// Returns the size.
    pub fn size(&self) -> usize {
        self.0.len()
    }

    /// Resizes the blocks hole to a new length.
    ///
    /// If `new_len` is greater than current length, the new blocks are all marked as hole.
    pub fn resize(&mut self, new_len: u32) {
        self.0.resize(new_len as usize, true);
    }

    /// Returns if the block `idx` is a hole.
    ///
    /// # Panic
    ///
    /// If the `idx` is out of bounds, this method will panic.
    pub fn is_hole(&self, idx: u32) -> bool {
        self.0[idx as usize]
    }

    /// Marks the block `idx` as a hole.
    ///
    /// # Panic
    ///
    /// If the `idx` is out of bounds, this method will panic.
    pub fn set(&mut self, idx: u32) {
        self.0.set(idx as usize, true);
    }

    /// Unmarks the block `idx` as a hole.
    ///
    /// # Panic
    ///
    /// If the `idx` is out of bounds, this method will panic.
    pub fn unset(&mut self, idx: u32) {
        self.0.set(idx as usize, false);
    }
}
