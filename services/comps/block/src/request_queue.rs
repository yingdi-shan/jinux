use crate::prelude::*;

use super::{
    bio::{BioEnqueueError, BioType, SubmitBio},
    id::Sid,
};

/// Represents a queue for `BioRequest` objects.
pub trait BioRequestQueue {
    /// Enqueues a `SubmitBio` to this queue.
    ///
    /// This `SubmitBio` will be merged into an existing `BioRequest`, or a new
    /// `BioRequest` will be created before being placed into the queue.
    ///
    /// This method will wake up the waiter if a new request is enqueued.
    fn enqueue(&self, bio: SubmitBio) -> Result<(), BioEnqueueError>;

    /// Dequeues a `BioRequest` from this queue.
    ///
    /// This method will wait until one request can be retrieved.
    fn dequeue(&self) -> BioRequest;
}

/// The block I/O request.
pub struct BioRequest {
    /// The type of the I/O
    type_: BioType,
    /// The range of target sectors on the device
    sid_range: Range<Sid>,
    /// The submitted bios
    bios: VecDeque<SubmitBio>,
}

impl BioRequest {
    /// Returns the type of the I/O.
    pub fn type_(&self) -> BioType {
        self.type_
    }

    /// Returns the range of sector id on device.
    pub fn sid_range(&self) -> &Range<Sid> {
        &self.sid_range
    }

    /// Returns the reference to the submitted `SubmitBio`s.
    pub fn bios(&self) -> &VecDeque<SubmitBio> {
        &self.bios
    }

    /// Returns `true` if can merge the `rq_bio`, `false` otherwise.
    pub fn can_merge(&self, rq_bio: &SubmitBio) -> bool {
        if rq_bio.type_() != self.type_ {
            return false;
        }

        rq_bio.sid_range().start == self.sid_range.end
            || rq_bio.sid_range().end == self.sid_range.start
    }

    /// Merges the `rq_bio` into this request.
    /// The merged `rq_bio` can only be placed at the front or back.
    ///
    /// # Panic
    ///
    /// If the `rq_bio` can not be merged, this method will panic.
    pub fn merge_bio(&mut self, rq_bio: SubmitBio) {
        assert!(self.can_merge(&rq_bio));

        if rq_bio.sid_range().start == self.sid_range.end {
            // Merge back
            self.sid_range.end = rq_bio.sid_range().end;
            self.bios.push_back(rq_bio);
        } else {
            // Merge front
            self.sid_range.start = rq_bio.sid_range().start;
            self.bios.push_front(rq_bio);
        }
    }
}

impl From<SubmitBio> for BioRequest {
    fn from(bio: SubmitBio) -> Self {
        Self {
            type_: bio.type_(),
            sid_range: bio.sid_range().clone(),
            bios: {
                let mut bios = VecDeque::with_capacity(1);
                bios.push_front(bio);
                bios
            },
        }
    }
}
