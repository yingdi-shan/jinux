use core::mem::size_of;
use jinux_frame::vm::VmFrame;

use super::{
    bitmap::{ExfatBitmap, EXFAT_RESERVED_CLUSTERS},
    constants::EXFAT_FIRST_CLUSTER,
    fs::ExfatFS,
};
use crate::prelude::*;

pub type ClusterID = u32;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FatValue {
    Free,
    Next(ClusterID),
    Bad,
    EndOfChain,
}

pub const EXFAT_EOF_CLUSTER: ClusterID = 0xFFFFFFFF;
pub const EXFAT_BAD_CLUSTER: ClusterID = 0xFFFFFFF7;
pub const EXFAT_FREE_CLUSTER: ClusterID = 0;
pub const FAT_ENTRY_SIZE: usize = size_of::<ClusterID>();

impl From<ClusterID> for FatValue {
    fn from(value: ClusterID) -> Self {
        match value {
            EXFAT_BAD_CLUSTER => FatValue::Bad,
            EXFAT_FREE_CLUSTER => FatValue::Free,
            EXFAT_EOF_CLUSTER => FatValue::EndOfChain,
            _ => FatValue::Next(value),
        }
    }
}

impl From<FatValue> for ClusterID {
    fn from(val: FatValue) -> Self {
        match val {
            FatValue::Free => EXFAT_FREE_CLUSTER,
            FatValue::EndOfChain => EXFAT_EOF_CLUSTER,
            FatValue::Bad => EXFAT_BAD_CLUSTER,
            FatValue::Next(x) => x,
        }
    }
}

bitflags! {
    #[derive(Default)]
    pub struct FatChainFlags:u8 {
        //An associated allocation of clusters is possible
        const ALLOC_POSSIBLE = 0x01;
        //The allocated clusters are contiguous and fat table is irrevalent.
        const FAT_CHAIN_NOT_IN_USE = 0x03;
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExfatChain {
    // current clusterID
    current: ClusterID,
    num_clusters: u32,
    // use FAT or not
    flags: FatChainFlags,
    fs: Weak<ExfatFS>,
}

//A position by the chain and relative offset in the cluster.
pub type ExfatChainPosition = (ExfatChain, usize);

impl ExfatChain {
    pub fn new(
        fs: Weak<ExfatFS>,
        current: ClusterID,
        num_clusters: Option<u32>,
        flags: FatChainFlags,
    ) -> Result<Self> {
        let mut chain = Self {
            current,
            num_clusters: 0,
            flags,
            fs,
        };

        let clusters = {
            if let Some(clu) = num_clusters {
                clu
            } else {
                chain.count_clusters()?
            }
        };

        chain.num_clusters = clusters;

        Ok(chain)
    }

    pub(super) fn cluster_size(&self) -> usize {
        self.fs().cluster_size()
    }

    pub fn num_clusters(&self) -> u32 {
        self.num_clusters
    }

    pub fn cluster_id(&self) -> ClusterID {
        self.current
    }

    pub fn flags(&self) -> FatChainFlags {
        self.flags
    }

    fn fat_in_use(&self) -> bool {
        !self.flags().contains(FatChainFlags::FAT_CHAIN_NOT_IN_USE)
    }

    pub(super) fn set_flags(&mut self, flags: FatChainFlags) {
        self.flags = flags;
    }

    fn fs(&self) -> Arc<ExfatFS> {
        self.fs.upgrade().unwrap()
    }

    pub(super) fn physical_cluster_start_offset(&self) -> usize {
        let cluster_num = (self.current - EXFAT_RESERVED_CLUSTERS) as usize;
        (cluster_num * self.cluster_size())
            + self.fs().super_block().data_start_sector as usize
                * self.fs().super_block().sector_size as usize
    }

    //Walk to the cluster at the given offset, return the new relative offset
    pub fn walk_to_cluster_at_offset(&self, offset: usize) -> Result<ExfatChainPosition> {
        let cluster_size = self.fs().cluster_size();
        let steps = offset / cluster_size;
        let result_chain = self.walk(steps as u32)?;
        let result_offset = offset % cluster_size;
        Ok((result_chain, result_offset))
    }

    pub fn is_current_cluster_valid(&self) -> bool {
        self.fs().is_valid_cluster(self.current)
    }

    //When the num_clusters is unknown, we need to count it from the begin.
    fn count_clusters(&self) -> Result<u32> {
        if !self.fat_in_use() {
            return_errno_with_message!(
                Errno::EIO,
                "Unable to count clusters when FAT table not in use."
            )
        } else {
            let mut cluster = self.current;
            let mut cnt = 1;
            loop {
                let fat = self.fs().read_next_fat(cluster)?;
                match fat {
                    FatValue::Next(next_fat) => {
                        cluster = next_fat;
                        cnt += 1;
                    }
                    _ => {
                        return Ok(cnt);
                    }
                }
            }
        }
    }

    //The destination cluster must be a valid cluster.
    pub fn walk(&self, steps: u32) -> Result<ExfatChain> {
        if steps > self.num_clusters {
            return_errno_with_message!(Errno::EINVAL, "invalid walking steps for FAT chain")
        }

        let mut result_cluster = self.current;
        if !self.fat_in_use() {
            result_cluster = (result_cluster + steps) as ClusterID;
        } else {
            for _ in 0..steps {
                let fat = self.fs().read_next_fat(result_cluster)?;
                match fat {
                    FatValue::Next(next_fat) => result_cluster = next_fat,
                    _ => return_errno_with_message!(Errno::EIO, "invalid access to FAT cluster"),
                }
            }
        }

        ExfatChain::new(
            self.fs.clone(),
            result_cluster,
            Some(self.num_clusters - steps),
            self.flags,
        )
    }

    //FIXME: What if cluster size is smaller than page size?

    ///Offset must be inside this cluster
    pub fn read_page(&self, offset: usize, page: &VmFrame) -> Result<()> {
        if offset + PAGE_SIZE > self.cluster_size() {
            return_errno_with_message!(
                Errno::EINVAL,
                "ExfatChain failed to read page: out of boundary."
            )
        }

        let physical_offset = self.physical_cluster_start_offset() + offset;
        self.fs()
            .block_device()
            .read_page(physical_offset / PAGE_SIZE, page)
    }

    ///Offset must be inside this cluster
    pub fn write_page(&self, offset: usize, page: &VmFrame) -> Result<()> {
        if offset + PAGE_SIZE > self.cluster_size() {
            return_errno_with_message!(
                Errno::EINVAL,
                "ExfatChain failed to read page: out of boundary."
            )
        }

        let physical_offset = self.physical_cluster_start_offset() + offset;
        self.fs()
            .block_device()
            .write_page(physical_offset / PAGE_SIZE, page)
    }

    //FIXME: Code repetition for read_at and write_at.
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let (mut chain, mut off_in_cluster) = self.walk_to_cluster_at_offset(offset)?;
        let mut bytes_read = 0usize;

        while bytes_read < buf.len() {
            let physical_offset = chain.physical_cluster_start_offset() + off_in_cluster;
            let to_read_size = (buf.len() - bytes_read).min(self.cluster_size() - off_in_cluster);

            let read_size = self.fs().block_device().read_at(
                physical_offset,
                &mut buf[bytes_read..bytes_read + to_read_size],
            )?;

            bytes_read += read_size;
            off_in_cluster += read_size;

            if off_in_cluster == self.cluster_size() {
                chain = chain.walk(1)?;
                off_in_cluster = 0;
            }
        }

        Ok(bytes_read)
    }
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let (mut chain, mut off_in_cluster) = self.walk_to_cluster_at_offset(offset)?;
        let mut bytes_written = 0usize;

        while bytes_written < buf.len() {
            let physical_offset = chain.physical_cluster_start_offset() + off_in_cluster;
            let to_write_size =
                (buf.len() - bytes_written).min(self.cluster_size() - off_in_cluster);

            let write_size = self.fs().block_device().write_at(
                physical_offset,
                &buf[bytes_written..bytes_written + to_write_size],
            )?;

            bytes_written += write_size;
            off_in_cluster += write_size;

            if off_in_cluster == self.cluster_size() {
                chain = chain.walk(1)?;
                off_in_cluster = 0;
            }
        }

        Ok(bytes_written)
    }

    // If current capacity is 0(no start_cluster), this means we can choose a allocation type
    // We first try continuous allocation
    // If no continuous allocation available, turn to fat allocation
    fn alloc_cluster_from_empty(
        &mut self,
        num_to_be_allocated: u32,
        bitmap: &mut SpinLockGuard<'_, ExfatBitmap>,
        sync_bitmap: bool,
    ) -> Result<ClusterID> {
        // search for a continuous chunk big enough
        let search_result =
            bitmap.find_next_free_cluster_range_fast(EXFAT_FIRST_CLUSTER, num_to_be_allocated);

        if let Ok(clusters) = search_result {
            bitmap.set_bitmap_range_used(clusters.clone(), sync_bitmap)?;
            self.current = clusters.start;
            self.flags = FatChainFlags::FAT_CHAIN_NOT_IN_USE;
            Ok(clusters.start)
        } else {
            let allocated_start_cluster =
                self.alloc_cluster_fat(num_to_be_allocated, sync_bitmap, bitmap)?;
            self.current = allocated_start_cluster;
            self.flags = FatChainFlags::ALLOC_POSSIBLE;
            Ok(allocated_start_cluster)
        }
    }

    // allocate clusters in fat mode, return the first allocated cluster id. Bitmap need to be already locked.
    fn alloc_cluster_fat(
        &mut self,
        num_to_be_allocated: u32,
        sync_bitmap: bool,
        bitmap: &mut SpinLockGuard<'_, ExfatBitmap>,
    ) -> Result<ClusterID> {
        let fs = self.fs();
        let mut alloc_start_cluster = 0;
        let mut prev_cluster = 0;
        let mut cur_cluster = EXFAT_FIRST_CLUSTER;
        for i in 0..num_to_be_allocated {
            cur_cluster = bitmap.find_next_free_cluster(cur_cluster)?;
            bitmap.set_bitmap_used(cur_cluster, sync_bitmap)?;

            if i == 0 {
                alloc_start_cluster = cur_cluster;
            } else {
                fs.write_next_fat(prev_cluster, FatValue::Next(cur_cluster))?;
            }

            prev_cluster = cur_cluster;
        }
        fs.write_next_fat(prev_cluster, FatValue::EndOfChain)?;
        Ok(alloc_start_cluster)
    }

    fn remove_cluster_fat(
        &mut self,
        start_physical_cluster: ClusterID,
        drop_num: u32,
        sync_bitmap: bool,
        bitmap: &mut SpinLockGuard<'_, ExfatBitmap>,
    ) -> Result<()> {
        let fs = self.fs();

        let mut cur_cluster = start_physical_cluster;
        for i in 0..drop_num {
            bitmap.set_bitmap_unused(cur_cluster, sync_bitmap)?;
            match fs.read_next_fat(cur_cluster)? {
                FatValue::Next(data) => {
                    cur_cluster = data;
                    if i == drop_num - 1 {
                        return_errno_with_message!(Errno::EINVAL, "invalid fat entry")
                    }
                }
                FatValue::EndOfChain => {
                    if i != drop_num - 1 {
                        return_errno_with_message!(Errno::EINVAL, "invalid fat entry")
                    }
                }
                _ => return_errno_with_message!(Errno::EINVAL, "invalid fat entry"),
            }
        }

        Ok(())
    }
}

pub trait ClusterAllocator {
    fn extend_clusters(&mut self, num_to_be_allocated: u32, sync_bitmap: bool)
        -> Result<ClusterID>;
    fn remove_clusters_from_tail(&mut self, free_num: u32, sync_bitmap: bool) -> Result<()>;
}

impl ClusterAllocator for ExfatChain {
    // Append clusters at the end of the chain, return the first allocated cluster
    // Caller should update size_allocated accordingly.
    // The file system must be locked before calling.
    fn extend_clusters(
        &mut self,
        num_to_be_allocated: u32,
        sync_bitmap: bool,
    ) -> Result<ClusterID> {
        let fs = self.fs();

        let bitmap_binding = fs.bitmap();
        let mut bitmap = bitmap_binding.lock();

        if num_to_be_allocated > bitmap.free_clusters() {
            return_errno!(Errno::ENOSPC)
        }

        if self.num_clusters == 0 {
            let allocated =
                self.alloc_cluster_from_empty(num_to_be_allocated, &mut bitmap, sync_bitmap)?;
            self.num_clusters += num_to_be_allocated;
            return Ok(allocated);
        }

        let start_cluster = self.cluster_id();
        let num_clusters = self.num_clusters;

        // Try to alloc contiguously otherwise break the chain.
        if !self.fat_in_use() {
            // first, check if there are enough following clusters.
            // if not, we can give up continuous allocation and turn to fat allocation
            let current_end = start_cluster + num_clusters;
            let clusters = current_end..current_end + num_to_be_allocated;
            if bitmap.is_cluster_range_free(clusters.clone())? {
                // Considering that the following clusters may be out of range, we should deal with this error here(just turn to fat allocation)
                bitmap.set_bitmap_range_used(clusters, sync_bitmap)?;
                self.num_clusters += num_to_be_allocated;
                return Ok(start_cluster);
            } else {
                // break the chain.
                for i in 0..num_clusters - 1 {
                    fs.write_next_fat(start_cluster + i, FatValue::Next(start_cluster + i + 1))?;
                }
                fs.write_next_fat(start_cluster + num_clusters - 1, FatValue::EndOfChain)?;
                self.set_flags(FatChainFlags::ALLOC_POSSIBLE);
            }
        }

        //Allocate remaining clusters the tail.
        let allocated_start_cluster =
            self.alloc_cluster_fat(num_to_be_allocated, sync_bitmap, &mut bitmap)?;

        //Insert allocated clusters to the tail.
        let tail_cluster = self.walk(num_clusters - 1)?.cluster_id();
        fs.write_next_fat(tail_cluster, FatValue::Next(allocated_start_cluster))?;

        self.num_clusters += num_to_be_allocated;

        Ok(allocated_start_cluster)
    }

    fn remove_clusters_from_tail(&mut self, drop_num: u32, sync_bitmap: bool) -> Result<()> {
        let fs = self.fs();

        let num_clusters = self.num_clusters;
        if drop_num > num_clusters {
            return_errno_with_message!(Errno::EINVAL, "invalid free_num.")
        }

        let trunc_start_cluster = self.walk(num_clusters - drop_num)?.cluster_id();

        let bitmap_binding = fs.bitmap();
        let mut bitmap = bitmap_binding.lock();

        if !self.fat_in_use() {
            bitmap.set_bitmap_range_unused(
                trunc_start_cluster..trunc_start_cluster + drop_num,
                sync_bitmap,
            )?;
        } else {
            self.remove_cluster_fat(trunc_start_cluster, drop_num, sync_bitmap, &mut bitmap)?;
            if drop_num != num_clusters {
                let tail_cluster = self.walk(num_clusters - drop_num - 1)?.cluster_id();
                self.fs()
                    .write_next_fat(tail_cluster, FatValue::EndOfChain)?;
            }
        }

        self.num_clusters -= drop_num;

        Ok(())
    }
}
