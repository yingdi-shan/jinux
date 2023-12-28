use crate::prelude::*;

use super::{id::Sid, BlockDevice, BLOCK_SIZE, SECTOR_SIZE};

use int_to_c_enum::TryFromInt;
use jinux_frame::{
    sync::WaitQueue,
    vm::{VmFrame, VmReader, VmSegment, VmWriter},
};

/// The unit for block I/O.
///
/// Each `Bio` packs the following information:
/// (1) The type of the I/O,
/// (2) The target sectors on the device for doing I/O,
/// (3) The memory locations (`BioSegment`) from/to which data are read/written,
/// (4) The optional callback function that will be invoked when the I/O is completed.
pub struct Bio(Arc<BioInner>);

impl Bio {
    /// Constructs a new `Bio`.
    ///
    /// The `type_` describes the type of the I/O.
    /// The `start_sid` is the starting sector id on the device.
    /// The `segments` describes the memory segments.
    /// The `complete_fn` is the optional callback function.
    pub fn new(
        type_: BioType,
        start_sid: Sid,
        segments: Vec<BioSegment>,
        complete_fn: Option<fn(&SubmitBio)>,
    ) -> Self {
        let nsectors = segments
            .iter()
            .map(|segment| segment.nsectors().to_raw())
            .sum();

        let inner = Arc::new(BioInner {
            type_,
            sid_range: start_sid..start_sid + nsectors,
            segments,
            complete_fn,
            status: AtomicU32::new(BioStatus::Init as u32),
            wait_queue: WaitQueue::new(),
        });
        Self(inner)
    }

    /// Returns the type.
    pub fn type_(&self) -> BioType {
        self.0.type_()
    }

    /// Returns the range of target sectors on the device.
    pub fn sid_range(&self) -> &Range<Sid> {
        self.0.sid_range()
    }

    /// Returns the memory segments.
    pub fn segments(&self) -> &Vec<BioSegment> {
        self.0.segments()
    }

    /// Returns the status.
    pub fn status(&self) -> BioStatus {
        self.0.status()
    }

    /// Submits self to the `block_device` asynchronously.
    ///
    /// Returns a `BioComplete` to the caller to wait for the completion.
    pub fn submit(&self, block_device: &dyn BlockDevice) -> Result<BioComplete, BioEnqueueError> {
        // Change the status from "Init" to "Submit".
        let result = self.0.status.compare_exchange(
            BioStatus::Init as u32,
            BioStatus::Submit as u32,
            Ordering::Release,
            Ordering::Relaxed,
        );
        assert!(result.is_ok());

        if let Err(e) = block_device
            .request_queue()
            .enqueue(SubmitBio(self.0.clone()))
        {
            // Fail to submit, revert the status.
            let result = self.0.status.compare_exchange(
                BioStatus::Submit as u32,
                BioStatus::Init as u32,
                Ordering::Release,
                Ordering::Relaxed,
            );
            assert!(result.is_ok());
            return Err(e);
        }

        Ok(BioComplete {
            bios: vec![self.0.clone()],
        })
    }

    /// Submits self to the `block_device` and waits for the result synchronously.
    ///
    /// Returns the result status of the `Bio`.
    pub fn submit_sync(
        &self,
        block_device: &dyn BlockDevice,
    ) -> Result<BioStatus, BioEnqueueError> {
        let complete = self.submit(block_device)?;
        let status = complete.wait().pop().unwrap();
        Ok(status)
    }
}

/// The error type returned when enqueueing the `Bio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BioEnqueueError {
    /// The request queue is full
    IsFull,
    /// Refuse to enqueue the bio
    Refused,
}

impl From<BioEnqueueError> for jinux_frame::Error {
    fn from(_error: BioEnqueueError) -> Self {
        jinux_frame::Error::NotEnoughResources
    }
}

/// A structure representing the completion of `Bio` submissions.
///
/// This structure holds a collection of `Bio` objects and provides functionality to
/// wait for their completion and retrieve their statuses.
pub struct BioComplete {
    bios: Vec<Arc<BioInner>>,
}

impl BioComplete {
    /// Constructs a new `BioComplete` instance with no `Bio` objects.
    pub fn new() -> Self {
        Self { bios: Vec::new() }
    }

    /// Merges the `Bio` objects from another `BioComplete` into this one.
    ///
    /// The another `BioComplete`'s `Bio` objects are appended to the end of
    /// the `Bio` vector of `self`, effectively concatenating the two collections.
    pub fn concat(&mut self, mut other: Self) {
        self.bios.append(&mut other.bios);
    }

    /// Waits for the completion of all `Bio` objects and returns their statuses.
    ///
    /// This method iterates through each `Bio` in the vector, waiting for their
    /// completion. The statuses of all completed `Bio` objects are returned
    /// in a vector.
    pub fn wait(&self) -> Vec<BioStatus> {
        self.bios
            .iter()
            .map(|bio| {
                bio.wait_queue.wait_until(|| {
                    let status = bio.status();
                    if status != BioStatus::Submit {
                        Some(status)
                    } else {
                        None
                    }
                })
            })
            .collect()
    }
}

impl Default for BioComplete {
    fn default() -> Self {
        Self::new()
    }
}

/// A Submitted Bio.
///
/// The request queue of block device only accepts a `SubmitBio` into the queue.
pub struct SubmitBio(Arc<BioInner>);

impl SubmitBio {
    /// Returns the type.
    pub fn type_(&self) -> BioType {
        self.0.type_()
    }

    /// Returns the range of target sectors on the device.
    pub fn sid_range(&self) -> &Range<Sid> {
        self.0.sid_range()
    }

    /// Returns the memory segments.
    pub fn segments(&self) -> &Vec<BioSegment> {
        self.0.segments()
    }

    /// Returns the status.
    pub fn status(&self) -> BioStatus {
        self.0.status()
    }

    /// Completes the I/O with the `status` and invokes the callback function.
    ///
    /// When the driver finishes the request for this `Bio`, it will call this method.
    pub fn complete(&self, status: BioStatus) {
        assert!(status != BioStatus::Init && status != BioStatus::Submit);

        // Set the status.
        let result = self.0.status.compare_exchange(
            BioStatus::Submit as u32,
            status as u32,
            Ordering::Release,
            Ordering::Relaxed,
        );
        assert!(result.is_ok());

        self.0.wait_queue.wake_one();
        if let Some(complete_fn) = self.0.complete_fn {
            complete_fn(self);
        }
    }
}

/// The common inner part of `Bio`.
struct BioInner {
    /// The type of the I/O
    type_: BioType,
    /// The range of the sector id on device
    sid_range: Range<Sid>,
    /// The memory segments in this `Bio`
    segments: Vec<BioSegment>,
    /// The I/O completion method
    complete_fn: Option<fn(&SubmitBio)>,
    /// The I/O status
    status: AtomicU32,
    /// The wait queue for I/O completion
    wait_queue: WaitQueue,
}

impl BioInner {
    pub fn type_(&self) -> BioType {
        self.type_
    }

    pub fn sid_range(&self) -> &Range<Sid> {
        &self.sid_range
    }

    pub fn segments(&self) -> &Vec<BioSegment> {
        &self.segments
    }

    pub fn status(&self) -> BioStatus {
        BioStatus::try_from(self.status.load(Ordering::Relaxed)).unwrap()
    }
}

/// The type of `Bio`.
#[derive(Clone, Copy, Debug, PartialEq, TryFromInt)]
#[repr(u8)]
pub enum BioType {
    /// Read sectors from the device.
    Read = 0,
    /// Write sectors into the device.
    Write = 1,
    /// Flush the volatile write cache.
    Flush = 2,
    /// Discard sectors.
    Discard = 3,
}

/// `BioSegment` is a smallest memory unit in block I/O.
///
/// It is a contiguous memory region that contains multiple sectors.
#[derive(Debug, Clone)]
pub struct BioSegment {
    /// The contiguous pages on which this segment resides.
    pages: Pages,
    /// The offset relative to the first page.
    offset: usize,
    /// The length, may cross pages.
    len: usize,
}

#[derive(Debug, Clone)]
enum Pages {
    Frame(VmFrame),
    Segment(VmSegment),
}

impl<'a> BioSegment {
    /// Constructs a new `BioSegment` from `VmSegment`.
    pub fn from_segment(segment: VmSegment, offset: usize, len: usize) -> Self {
        assert!(offset % SECTOR_SIZE == 0);
        assert!(len % SECTOR_SIZE == 0);
        assert!(offset + len <= segment.nbytes());

        Self {
            pages: Pages::Segment(segment),
            offset,
            len,
        }
    }

    /// Constructs a new `BioSegment` from `VmFrame`.
    pub fn from_frame(frame: VmFrame, offset: usize, len: usize) -> Self {
        assert!(offset % SECTOR_SIZE == 0);
        assert!(len % SECTOR_SIZE == 0);
        assert!(offset + len <= BLOCK_SIZE);

        Self {
            pages: Pages::Frame(frame),
            offset,
            len,
        }
    }

    /// Returns the number of sectors.
    pub fn nsectors(&self) -> Sid {
        Sid::from_offset(self.len)
    }

    /// Returns the number of bytes.
    pub fn nbytes(&self) -> usize {
        self.len
    }

    /// Returns a reader to read data from it.
    pub fn reader(&'a self) -> VmReader<'a> {
        let reader = match &self.pages {
            Pages::Segment(segment) => segment.reader(),
            Pages::Frame(frame) => frame.reader(),
        };
        reader.skip(self.offset).limit(self.len)
    }

    /// Returns a writer to write data into it.
    pub fn writer(&'a self) -> VmWriter<'a> {
        let writer = match &self.pages {
            Pages::Segment(segment) => segment.writer(),
            Pages::Frame(frame) => frame.writer(),
        };
        writer.skip(self.offset).limit(self.len)
    }
}

/// The status of `Bio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, TryFromInt)]
#[repr(u32)]
pub enum BioStatus {
    /// The initial status for a newly created `Bio`.
    Init = 0,
    /// After a `Bio` is submitted, its status will be changed to "Submit".
    Submit = 1,
    /// The I/O operation has been successfully completed.
    Complete = 2,
    /// The I/O operation is not supported.
    NotSupported = 3,
    /// Insufficient space is available to perform the I/O operation.
    NoSpace = 4,
    /// An error occurred while doing I/O.
    IoError = 5,
}

pub fn general_complete_fn(bio: &SubmitBio) {
    match bio.status() {
        BioStatus::Complete => (),
        err_status => log::error!(
            "faild to do {:?} on the device with error status: {:?}",
            bio.type_(),
            err_status
        ),
    }
}
