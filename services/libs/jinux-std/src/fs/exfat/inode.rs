use crate::fs::exfat::dentry::ExfatDentryIterator;
use crate::fs::exfat::fat::ExfatChain;

use crate::fs::exfat::fs::ExfatFS;
use crate::fs::utils::{Inode, InodeType, Metadata, PageCache, PageCacheBackend};
use crate::time::now_as_duration;

use super::block_device::{is_block_aligned, SECTOR_SIZE};
use super::constants::*;
use super::dentry::{
    Checksum, ExfatDentry, ExfatDentrySet, ExfatFileDentry, ExfatName, DENTRY_SIZE,
};
use super::fat::{ClusterAllocator, ClusterID, ExfatChainPosition, FatChainFlags};
use super::fs::{ExfatMountOptions, EXFAT_ROOT_INO};
use super::utils::{make_hash_index, DosTimestamp};
use crate::events::IoEvents;
use crate::fs::device::Device;
use crate::fs::utils::DirentVisitor;
use crate::fs::utils::InodeMode;
use crate::fs::utils::IoctlCmd;
use crate::prelude::*;
use crate::process::signal::Poller;
use crate::vm::vmo::Vmo;
pub(super) use align_ext::AlignExt;
use alloc::string::String;
use core::cmp::Ordering;
use core::time::Duration;
use jinux_frame::vm::{VmAllocOptions, VmFrame, VmIo};
use jinux_rights::Full;

///Inode number
pub type Ino = usize;

bitflags! {
    pub struct FatAttr : u16{
        const READONLY = 0x0001;
        const HIDDEN   = 0x0002;
        const SYSTEM   = 0x0004;
        const VOLUME   = 0x0008;
        const DIRECTORY   = 0x0010;
        ///This file has been touched since the last DOS backup was performed on it.
        const ARCHIVE  = 0x0020;
    }
}

impl FatAttr {
    /* Convert attribute bits and a mask to the UNIX mode. */
    fn make_mode(&self, mount_option: ExfatMountOptions, mode: InodeMode) -> InodeMode {
        let mut ret = mode;
        if self.contains(FatAttr::READONLY) && !self.contains(FatAttr::DIRECTORY) {
            ret.remove(InodeMode::S_IWGRP | InodeMode::S_IWUSR | InodeMode::S_IWOTH);
        }
        if self.contains(FatAttr::DIRECTORY) {
            ret.remove(InodeMode::from_bits_truncate(mount_option.fs_dmask));
        } else {
            ret.remove(InodeMode::from_bits_truncate(mount_option.fs_fmask));
        }
        ret
    }
}

#[derive(Debug)]
pub struct ExfatInode(RwLock<ExfatInodeInner>);

impl ExfatInode {
    pub(super) fn hash_index(&self) -> usize {
        self.0.read().hash_index()
    }

    fn count_num_subdir(start_chain: ExfatChain, size: usize) -> Result<usize> {
        if !start_chain.is_current_cluster_valid() {
            return Ok(0);
        }
        //FIXME:What if there are some invalid dentries in volume?
        let iterator = ExfatDentryIterator::new(start_chain, 0, Some(size))?;
        let mut cnt = 0;
        for dentry_result in iterator {
            let dentry = dentry_result?;
            if let ExfatDentry::File(_) = dentry {
                cnt += 1
            }
        }
        Ok(cnt)
    }

    pub(super) fn build_root_inode(
        fs_weak: Weak<ExfatFS>,
        root_chain: ExfatChain,
    ) -> Result<Arc<ExfatInode>> {
        let sb = fs_weak.upgrade().unwrap().super_block();

        let root_cluster = sb.root_dir;

        let dentry_set_size = 0;

        let attr = FatAttr::DIRECTORY;

        let inode_type = InodeType::Dir;

        let ctime =
            DosTimestamp::from_duration(now_as_duration(&crate::time::ClockID::CLOCK_REALTIME)?)?;

        let size = root_chain.num_clusters() as usize * sb.cluster_size as usize;

        let name = ExfatName::new();

        let num_subdir = Self::count_num_subdir(root_chain.clone(), size)? as u32;

        Ok(Arc::new_cyclic(|weak_self| {
            ExfatInode(RwLock::new(ExfatInodeInner {
                ino: EXFAT_ROOT_INO,
                dentry_set_position: ExfatChainPosition::default(),
                dentry_set_size: 0,
                dentry_entry: 0,
                inode_type,
                attr,
                start_chain: root_chain,
                size,
                size_allocated: size,
                atime: ctime,
                mtime: ctime,
                ctime,
                num_subdir,
                name,
                page_cache: PageCache::with_capacity(size, weak_self.clone() as _).unwrap(),
                fs: fs_weak,
            }))
        }))
    }

    fn build_inode_from_dentry_set(
        fs: Arc<ExfatFS>,
        dentry_set: &ExfatDentrySet,
        dentry_set_position: ExfatChainPosition,
        dentry_entry: u32,
        ino: Ino,
    ) -> Result<Arc<ExfatInode>> {
        const EXFAT_MIMIMUM_DENTRY: usize = 3;

        if dentry_set.len() < EXFAT_MIMIMUM_DENTRY {
            return_errno_with_message!(Errno::EINVAL, "invalid dentry length")
        }

        let dentry_set_size = dentry_set.len() * DENTRY_SIZE;

        let fs_weak = Arc::downgrade(&fs);

        let file = dentry_set.get_file_dentry();
        let attr = FatAttr::from_bits_truncate(file.attribute);

        let inode_type = if attr.contains(FatAttr::DIRECTORY) {
            InodeType::Dir
        } else {
            InodeType::File
        };

        let ctime = DosTimestamp::new(
            file.create_time,
            file.create_date,
            file.create_time_cs,
            file.create_utc_offset,
        )?;
        let mtime = DosTimestamp::new(
            file.modify_time,
            file.modify_date,
            file.modify_time_cs,
            file.modify_utc_offset,
        )?;
        let atime = DosTimestamp::new(
            file.access_time,
            file.access_date,
            0,
            file.access_utc_offset,
        )?;

        let stream = dentry_set.get_stream_dentry();
        let size = stream.valid_size as usize;
        let size_allocated = stream.size as usize;

        if attr.contains(FatAttr::DIRECTORY) && size != size_allocated {
            return_errno_with_message!(
                Errno::EINVAL,
                "allocated_size and valid_size can only be different for files!"
            )
        }

        let chain_flag = FatChainFlags::from_bits_truncate(stream.flags);
        let start_cluster = stream.start_cluster;
        let num_clusters = size_allocated.align_up(fs.cluster_size()) / fs.cluster_size();

        let start_chain = ExfatChain::new(
            fs_weak.clone(),
            start_cluster,
            Some(num_clusters as u32),
            chain_flag,
        )?;

        let name = dentry_set.get_name()?;

        let num_subdir = if matches!(inode_type, InodeType::File) {
            0
        } else {
            Self::count_num_subdir(start_chain.clone(), size)? as u32
        };

        Ok(Arc::new_cyclic(|weak_self| {
            ExfatInode(RwLock::new(ExfatInodeInner {
                ino,
                dentry_set_position,
                dentry_set_size,
                dentry_entry,
                inode_type,
                attr,
                start_chain,
                size,
                size_allocated,
                atime,
                mtime,
                ctime,
                num_subdir,
                name,
                page_cache: PageCache::with_capacity(size, weak_self.clone() as _).unwrap(),
                fs: fs_weak,
            }))
        }))
    }

    //The caller of the function should give a unique ino to assign to the inode.
    pub(super) fn read_from_iterator(
        fs: Arc<ExfatFS>,
        dentry_entry: usize,
        chain_pos: ExfatChainPosition,
        file_dentry: &ExfatFileDentry,
        iter: &mut ExfatDentryIterator,
        ino: Ino,
    ) -> Result<Arc<Self>> {
        let dentry_set = ExfatDentrySet::read_from_iterator(file_dentry, iter)?;
        Self::build_inode_from_dentry_set(fs, &dentry_set, chain_pos, dentry_entry as u32, ino)
    }
}

/// In-memory rust object that represents a file or folder.
#[derive(Debug)]
pub struct ExfatInodeInner {
    /// Inode number
    ino: Ino,
    /// Dentry set position in its parent directory
    dentry_set_position: ExfatChainPosition,
    /// Dentry set size in bytes
    dentry_set_size: usize,
    /// The entry number of the dentry
    dentry_entry: u32,
    /// Inode type, File or Dir
    inode_type: InodeType,

    attr: FatAttr,

    /// Start position on disk, this is undefined if the allocated size is 0
    start_chain: ExfatChain,

    /// Valid size of the file
    size: usize,
    /// Allocated size, for directory, size is always equal to size_allocated.
    size_allocated: usize,

    /// Access time, updated after reading
    atime: DosTimestamp,
    /// Modification time, updated only on write
    mtime: DosTimestamp,
    /// Creation time.
    ctime: DosTimestamp,

    /// Number of sub directories
    num_subdir: u32,

    /// ExFAT uses UTF-16 encoding, rust use utf-8 for string processing.
    name: ExfatName,
    /// Page cache
    page_cache: PageCache,
    /// A pointer to exFAT fs
    fs: Weak<ExfatFS>,
}

struct EmptyVistor;
impl DirentVisitor for EmptyVistor {
    fn visit(&mut self, name: &str, ino: u64, type_: InodeType, offset: usize) -> Result<()> {
        Ok(())
    }
}

impl ExfatInodeInner {
    pub fn fs(&self) -> Arc<ExfatFS> {
        self.fs.upgrade().unwrap()
    }

    /// The hash_value to index inode. This should be unique in the whole fs.
    /// Currently use dentry set physical position as hash value except for root(0).
    pub fn hash_index(&self) -> usize {
        if self.ino == EXFAT_ROOT_INO {
            return ROOT_INODE_HASH;
        }

        make_hash_index(
            self.dentry_set_position.0.cluster_id(),
            self.dentry_set_position.1 as u32,
        )
    }

    pub fn make_mode(&self) -> InodeMode {
        self.attr
            .make_mode(self.fs().mount_option(), InodeMode::all())
    }

    /// Update inode information back to the disk for sync this inode.
    /// Should lock the file system before calling this function.
    fn write_inode(&mut self, sync: bool) -> Result<()> {
        // Root dir should not be updated.
        if self.ino == EXFAT_ROOT_INO {
            return Ok(());
        }

        // If the inode is already unlinked, there is no need for updating it.
        if !self.dentry_set_position.0.is_current_cluster_valid() {
            return Ok(());
        }

        // Need to read the latest dentry set.
        let mut dentry_set = ExfatDentrySet::read_from(&self.dentry_set_position)?;

        let mut file_dentry = dentry_set.get_file_dentry();
        let mut stream_dentry = dentry_set.get_stream_dentry();

        file_dentry.attribute = self.attr.bits();

        file_dentry.create_utc_offset = self.ctime.utc_offset;
        file_dentry.create_date = self.ctime.date;
        file_dentry.create_time = self.ctime.time;
        file_dentry.create_time_cs = self.ctime.increament_10ms;

        file_dentry.modify_utc_offset = self.mtime.utc_offset;
        file_dentry.modify_date = self.mtime.date;
        file_dentry.modify_time = self.mtime.time;
        file_dentry.modify_time_cs = self.mtime.increament_10ms;

        file_dentry.access_utc_offset = self.atime.utc_offset;
        file_dentry.access_date = self.atime.date;
        file_dentry.access_time = self.atime.time;

        if !self.start_chain.is_current_cluster_valid() {
            self.size = 0;
            self.size_allocated = 0;
        }

        stream_dentry.valid_size = self.size as u64;
        stream_dentry.size = self.size_allocated as u64;
        stream_dentry.start_cluster = self.start_chain.cluster_id();
        stream_dentry.flags = self.start_chain.flags().bits();

        dentry_set.set_file_dentry(&file_dentry);
        dentry_set.set_stream_dentry(&stream_dentry);
        dentry_set.update_checksum();

        dentry_set.write_at(&self.dentry_set_position)?;

        Ok(())
    }

    /// Get the physical cluster id from the logical cluster id in the inode.
    fn get_physical_cluster(&self, logical: ClusterID) -> Result<ClusterID> {
        let chain = self.start_chain.walk(logical)?;
        Ok(chain.cluster_id())
    }

    /// Allocated clusters number
    fn num_clusters(&self) -> u32 {
        self.start_chain.num_clusters()
    }

    /// Get the cluster id from the logical cluster id in the inode. If cluster not exist, allocate a new one.
    /// exFAT do not support holes in the file, so new clusters need to be allocated.
    /// The file system must be locked before calling.
    fn get_or_allocate_cluster(&mut self, logical: ClusterID, sync: bool) -> Result<ClusterID> {
        let num_clusters = self.num_clusters();
        if logical >= num_clusters {
            self.start_chain
                .extend_clusters(logical - num_clusters, sync)?;
        }

        self.get_physical_cluster(logical)
    }

    /// Resize current inode to new_size.
    /// The file system must be locked before calling.
    pub fn resize(&mut self, new_size: usize) -> Result<()> {
        let fs = self.fs();
        let cluster_size = fs.cluster_size();
        let num_clusters = self.num_clusters();

        let new_num_clusters = (new_size.align_up(cluster_size) / cluster_size) as u32;

        let sync = self.is_sync();

        match new_num_clusters.cmp(&num_clusters) {
            Ordering::Greater => {
                // New clusters should be allocated.
                self.start_chain
                    .extend_clusters(new_num_clusters - num_clusters, sync)?;
            }
            Ordering::Less => {
                // Some exist clusters should be truncated.
                self.start_chain
                    .remove_clusters_from_tail(num_clusters - new_num_clusters, sync)?;
                if new_size < self.size {
                    // Valid data is truncated.
                    self.size = new_size;
                }
            }
            _ => {}
        };
        self.size_allocated = new_size;

        // Sync this inode if necessary.
        if sync {
            self.write_inode(true)?;
        }

        Ok(())
    }

    /// Lock the file system and call resize
    pub fn lock_and_resize(&mut self, new_size: usize) -> Result<()> {
        let fs = self.fs();
        let guard = fs.lock();
        self.resize(new_size)
    }

    /// Get physical sector id from logical sector id.
    /// Similar to get_physical_cluster
    fn get_sector_id(&self, sector_id: usize) -> Result<usize> {
        let chain_offset = self
            .start_chain
            .walk_to_cluster_at_offset(sector_id * self.fs().sector_size())?;

        let sect_per_cluster = self.fs().super_block().sect_per_cluster as usize;
        let cluster_id = sector_id / sect_per_cluster;
        let cluster = self.get_physical_cluster((sector_id / sect_per_cluster) as ClusterID)?;

        let sec_offset = sector_id % (self.fs().super_block().sect_per_cluster as usize);
        Ok(self.fs().cluster_to_off(cluster) / self.fs().sector_size() + sec_offset)
    }

    /// Get the physical sector id from the logical sector id in the inode. If corresponding cluster not exist, allocate a new one.
    /// Similar to get_or_allocate_cluster.
    /// This function will lock the file system.
    fn lock_and_get_or_allocate_sector_id(
        &mut self,
        sector_id: usize,
        alloc_page: bool,
    ) -> Result<usize> {
        let binding = self.fs();
        let guard = binding.lock();

        let sector_size = self.fs().sector_size();

        let sector_per_page = PAGE_SIZE / sector_size;
        let mut sector_end = sector_id;
        if alloc_page {
            sector_end = sector_id.align_up(sector_per_page);
        }

        let last_sector = self.size_allocated / sector_size;

        let sync = self.is_sync();
        // Get or allocate corresponding cluster.
        // Cluster size must be larger than page_size.
        let cluster = self.get_or_allocate_cluster(
            sector_end as u32 / self.fs().super_block().sect_per_cluster,
            sync,
        )?;

        let sec_offset = sector_id % (self.fs().super_block().sect_per_cluster as usize);

        Ok(self.fs().cluster_to_off(cluster) / sector_size + sec_offset)
    }

    fn page_cache(&self) -> &PageCache {
        &self.page_cache
    }

    fn read_sector(&self, idx: usize, frame: &VmFrame) -> Result<()> {
        let sector_id = self.get_sector_id(idx)?;
        self.fs().block_device().read_sector(sector_id, frame)?;
        Ok(())
    }

    fn write_sector(&mut self, idx: usize, frame: &VmFrame) -> Result<()> {
        let sector_id = self.lock_and_get_or_allocate_sector_id(idx, false)?;
        self.fs().block_device().write_sector(sector_id, frame)?;
        Ok(())
    }

    fn is_sync(&self) -> bool {
        true
        //TODO:Judge whether sync is necessary.
    }

    /// Find empty dentry. If not found, expand the clusterchain.
    fn find_empty_dentry(&mut self, num_dentries: usize) -> Result<usize> {
        let fs = self.fs();
        let dentry_iterator =
            ExfatDentryIterator::new(self.start_chain.clone(), 0, Some(self.size))?;

        let mut cont_unused = 0;
        let mut entry_id = 0;

        for dentry_result in dentry_iterator {
            let dentry = dentry_result?;
            match dentry {
                ExfatDentry::UnUsed | ExfatDentry::Deleted(_) => {
                    cont_unused += 1;
                }
                _ => {
                    cont_unused = 0;
                }
            }
            if cont_unused >= num_dentries {
                return Ok(entry_id - (num_dentries - 1));
            }
            entry_id += 1;
        }

        // Empty entries not found, allocate new cluster
        if self.size >= MAX_EXFAT_DENTRIES as usize * DENTRY_SIZE {
            return_errno!(Errno::ENOSPC)
        }

        let cluster_size = self.fs().cluster_size();
        let cluster_to_be_allocated =
            (num_dentries * DENTRY_SIZE).align_up(cluster_size) / cluster_size;

        self.start_chain
            .extend_clusters(cluster_to_be_allocated as u32, self.is_sync())?;
        self.size_allocated += cluster_size * cluster_to_be_allocated;
        self.size = self.size_allocated;

        Ok(entry_id)
    }

    /// Add new dentries. Create a new file or folder.
    fn add_entry(
        &mut self,
        name: &str,
        inode_type: InodeType,
        mode: InodeMode,
    ) -> Result<Arc<ExfatInode>> {
        if name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }

        // Allocate new cluster if no cluster is associated.
        if !self.start_chain.is_current_cluster_valid() {
            self.start_chain.extend_clusters(1, self.is_sync())?;
            let cluster_size = self.fs().cluster_size();
            self.size_allocated += cluster_size;
            self.size = self.size_allocated;
        }

        // TODO: remove trailing periods of pathname.
        // Do not allow creation of files with names ending with period(s).

        let name_dentries = (name.len() + EXFAT_FILE_NAME_LEN - 1) / EXFAT_FILE_NAME_LEN;
        let num_dentries = name_dentries + 2; // FILE Entry + Stream Entry + Name Entry
        let entry = self.find_empty_dentry(num_dentries)? as u32;

        if inode_type.is_directory() && !self.fs().mount_option().zero_size_dir {
            // We need to resize the directory so that it contains at least 1 cluster if zero_size_dir is not enabled.
        }

        let dentry_set = ExfatDentrySet::from(self.fs().clone(), name, inode_type, mode)?;
        let pos = self
            .start_chain
            .walk_to_cluster_at_offset(entry as usize * DENTRY_SIZE)?;

        dentry_set.write_at(&pos)?;

        self.num_subdir += 1;

        ExfatInode::build_inode_from_dentry_set(
            self.fs(),
            &dentry_set,
            pos,
            entry,
            self.fs().alloc_inode_number(),
        )
    }

    /// Read multiple dentry sets from the given position(offset) in this directory.
    /// The number to read is given by dir_cnt.
    /// Return the new offset after read if success.
    fn read_multiple_dirs(
        &self,
        offset: usize,
        dir_cnt: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        if !self.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        if dir_cnt == 0 {
            return Ok(offset);
        }

        let fs = self.fs();
        let cluster_size = fs.cluster_size();

        let physical_cluster = self.get_physical_cluster((offset / cluster_size) as u32)?;
        let cluster_off = (offset % cluster_size) as u32;

        let dentry_position_start = self.start_chain.walk_to_cluster_at_offset(offset)?;

        let mut iter = ExfatDentryIterator::new(
            dentry_position_start.0.clone(),
            dentry_position_start.1,
            None,
        )?;
        let mut dir_read = 0;
        let mut current_off = offset;

        // We need to skip empty or deleted dentry.
        while dir_read < dir_cnt {
            let dentry_result = iter.next();

            if dentry_result.is_none() {
                return_errno_with_message!(Errno::ENOENT, "inode data not available")
            }

            let dentry = dentry_result.unwrap()?;

            if let ExfatDentry::File(file) = dentry {
                let inode = self.read_single_dir(&file, &mut iter, current_off, visitor)?;
                current_off += inode.0.read().dentry_set_size;
                dir_read += 1;
            } else {
                current_off += DENTRY_SIZE;
            }
        }

        Ok(current_off)
    }

    /// Read a dentry set at offset. Return the corresponding inode.
    /// Dirent visitor will extract information from the inode.
    fn read_single_dir(
        &self,
        file_dentry: &ExfatFileDentry,
        iter: &mut ExfatDentryIterator,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<Arc<ExfatInode>> {
        if !self.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        let fs = self.fs();
        let cluster_size = fs.cluster_size();

        let dentry_position = self.start_chain.walk_to_cluster_at_offset(offset)?;

        if let Some(child_inode) = fs.find_opened_inode(make_hash_index(
            dentry_position.0.cluster_id(),
            dentry_position.1 as u32,
        )) {
            // Inode already exists.
            let child_inner = child_inode.0.read();
            for i in 0..(child_inner.dentry_set_size / DENTRY_SIZE - 1) {
                let dentry_result = iter.next();
                if dentry_result.is_none() {
                    return_errno_with_message!(Errno::ENOENT, "inode data not available")
                }
                dentry_result.unwrap()?;
            }
            visitor.visit(
                &child_inner.name.to_string(),
                child_inner.ino as u64,
                child_inner.inode_type,
                offset,
            )?;

            Ok(child_inode.clone())
        } else {
            // Otherwise, create a new node and insert it to hash map.
            let ino = fs.alloc_inode_number();
            let child_inode = ExfatInode::read_from_iterator(
                fs.clone(),
                offset / DENTRY_SIZE,
                dentry_position,
                file_dentry,
                iter,
                ino,
            )?;
            let _ = fs.insert_inode(child_inode.clone());
            let child_inner = child_inode.0.read();
            visitor.visit(
                &child_inner.name.to_string(),
                ino as u64,
                child_inner.inode_type,
                offset,
            )?;
            Ok(child_inode.clone())
        }
    }

    /// Look up a target with "name", cur inode represent a dir
    /// Return (target inode, dentries start offset, dentries len)
    /// No inode should hold the write lock.
    fn lookup_by_name(&self, target_name: &str) -> Result<(Arc<ExfatInode>, usize, usize)> {
        let sub_dir = self.num_subdir;
        let mut name_and_offsets: Vec<(String, usize)> = vec![];

        impl DirentVisitor for Vec<(String, usize)> {
            fn visit(
                &mut self,
                name: &str,
                ino: u64,
                type_: InodeType,
                offset: usize,
            ) -> Result<()> {
                self.push((name.into(), offset));
                Ok(())
            }
        }

        self.read_multiple_dirs(0, sub_dir as usize, &mut name_and_offsets)?;

        for (name, offset) in name_and_offsets {
            if name.eq(target_name) {
                let chain_off = self.start_chain.walk_to_cluster_at_offset(offset)?;
                let hash = make_hash_index(chain_off.0.cluster_id(), chain_off.1 as u32);
                let inode = self.fs().find_opened_inode(hash).unwrap();

                let size = inode.0.read().dentry_set_size;
                return Ok((inode, offset, size));
            }
        }
        return_errno!(Errno::ENOENT)
    }

    /// Only valid for directory, check if the dir is empty
    fn is_empty_dir(&self) -> Result<bool> {
        if !self.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        Ok(self.num_subdir == 0)
    }

    fn delete_associated_secondary_clusters(&mut self, dentry: &ExfatDentry) -> Result<()> {
        let cluster_size = self.fs().cluster_size();
        match dentry {
            ExfatDentry::VendorAlloc(inner) => {
                if !self.fs().is_valid_cluster(inner.start_cluster) {
                    return Ok(());
                }
                let num_to_free = (inner.size as usize / cluster_size) as u32;
                ExfatChain::new(
                    self.fs.clone(),
                    inner.start_cluster,
                    Some(num_to_free),
                    FatChainFlags::ALLOC_POSSIBLE,
                )?
                .remove_clusters_from_tail(num_to_free, self.is_sync())?;
            }
            ExfatDentry::GenericSecondary(inner) => {
                if !self.fs().is_valid_cluster(inner.start_cluster) {
                    return Ok(());
                }
                let num_to_free = (inner.size as usize / cluster_size) as u32;
                ExfatChain::new(
                    self.fs.clone(),
                    inner.start_cluster,
                    Some(num_to_free),
                    FatChainFlags::ALLOC_POSSIBLE,
                )?
                .remove_clusters_from_tail(num_to_free, self.is_sync())?;
            }
            _ => {}
        };
        Ok(())
    }

    // Delete dentry set for current directory.
    fn delete_dentry_set(&mut self, offset: usize, len: usize) -> Result<()> {
        let mut buf = vec![0; len];
        self.start_chain.read_at(offset, &mut buf)?;

        let num_dentry = len / DENTRY_SIZE;

        let cluster_size = self.fs().cluster_size();
        for i in 0..num_dentry {
            let buf_offset = DENTRY_SIZE * i;
            // Delete cluster chain if needed
            let dentry = ExfatDentry::try_from(&buf[buf_offset..buf_offset + DENTRY_SIZE])?;
            self.delete_associated_secondary_clusters(&dentry)?;
            // Mark this dentry deleted
            buf[buf_offset] &= 0x7F;
        }
        self.start_chain.write_at(offset, &buf)?;

        self.num_subdir -= 1;
        // FIXME: We must make sure that there are no spare tailing clusters in a directory.
        Ok(())
    }

    /// Copy metadata from the given inode.
    fn copy_metadata_from(&mut self, inode: Arc<ExfatInode>) {
        let inner = inode.0.read();
        self.dentry_set_position = inner.dentry_set_position.clone();
        self.dentry_set_size = inner.dentry_set_size;
        self.dentry_entry = inner.dentry_entry;
        self.atime = inner.atime;
        self.ctime = inner.ctime;
        self.mtime = inner.mtime;
        self.name = ExfatName::from_str(&inner.name.to_string()).unwrap();
    }
}

impl PageCacheBackend for ExfatInode {
    fn read_page(&self, idx: usize, frame: &VmFrame) -> Result<()> {
        let inner = self.0.read();
        if inner.size < idx * PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "Invalid read size")
        }
        let sector_id = inner.get_sector_id(idx * PAGE_SIZE / inner.fs().sector_size())?;
        inner
            .fs()
            .block_device()
            .read_page(sector_id * inner.fs().sector_size() / PAGE_SIZE, frame)?;
        Ok(())
    }

    //What if block_size is not equal to page size?
    fn write_page(&self, idx: usize, frame: &VmFrame) -> Result<()> {
        let inner = self.0.read();
        let sector_size = inner.fs().sector_size();
        //FIXME: dead lock may occur here.

        let sector_id = inner.get_sector_id(idx * PAGE_SIZE / SECTOR_SIZE)?;

        //FIXME: We may need to truncate the file if write_page fails?
        inner
            .fs()
            .block_device()
            .write_page(sector_id * inner.fs().sector_size() / PAGE_SIZE, frame)?;

        Ok(())
    }

    fn num_pages(&self) -> usize {
        let inner = self.0.read();
        inner.size_allocated.align_up(PAGE_SIZE) / PAGE_SIZE
    }
}

impl Inode for ExfatInode {
    fn ino(&self) -> u64 {
        self.0.read().ino as u64
    }

    fn len(&self) -> usize {
        self.0.read().size
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        let mut inner = self.0.write();

        //We must resize the inode before resizing the pagecache, to avoid the flush of extra dirty pages.
        inner.lock_and_resize(new_size)?;

        inner.page_cache.pages().resize(new_size)
    }

    fn metadata(&self) -> crate::fs::utils::Metadata {
        let inner = self.0.read();
        let blk_size = inner.fs().super_block().sector_size as usize;

        let nlinks = if inner.inode_type.is_directory() {
            (inner.num_subdir + 2) as usize
        } else {
            1
        };

        Metadata {
            dev: 0,
            ino: inner.ino,
            size: inner.size,
            blk_size,
            blocks: (inner.size + blk_size - 1) / blk_size,
            atime: inner.atime.as_duration().unwrap_or_default(),
            mtime: inner.mtime.as_duration().unwrap_or_default(),
            ctime: inner.ctime.as_duration().unwrap_or_default(),
            type_: inner.inode_type,
            mode: inner.make_mode(),
            nlinks,
            uid: inner.fs().mount_option().fs_uid,
            gid: inner.fs().mount_option().fs_gid,
            //real device
            rdev: 0,
        }
    }

    fn type_(&self) -> InodeType {
        self.0.read().inode_type
    }

    fn mode(&self) -> InodeMode {
        self.0.read().make_mode()
    }

    fn set_mode(&self, mode: InodeMode) {
        //Mode Set not supported.
        todo!("Set inode to readonly")
    }

    fn atime(&self) -> Duration {
        self.0.read().atime.as_duration().unwrap_or_default()
    }

    fn set_atime(&self, time: Duration) {
        self.0.write().atime = DosTimestamp::from_duration(time).unwrap_or_default();
    }

    fn mtime(&self) -> Duration {
        self.0.read().mtime.as_duration().unwrap_or_default()
    }

    fn set_mtime(&self, time: Duration) {
        self.0.write().mtime = DosTimestamp::from_duration(time).unwrap_or_default();
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::fs::utils::FileSystem> {
        self.0.read().fs()
    }

    fn page_cache(&self) -> Option<Vmo<Full>> {
        Some(self.0.read().page_cache().pages())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let inner = self.0.read();
        if inner.inode_type.is_directory() {
            return_errno!(Errno::EISDIR)
        }
        let (read_off, read_len) = {
            let file_size = inner.size;
            let start = file_size.min(offset);
            let end = file_size.min(offset + buf.len());
            (start, end - start)
        };
        inner
            .page_cache
            .pages()
            .read_bytes(read_off, &mut buf[..read_len])?;

        Ok(read_len)
    }

    // The offset and the length of buffer must be multiples of the block size.
    fn read_direct_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let inner = self.0.read();
        if inner.inode_type.is_directory() {
            return_errno!(Errno::EISDIR)
        }
        if !is_block_aligned(offset) || !is_block_aligned(buf.len()) {
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let sector_size = inner.fs().sector_size();

        let (read_off, read_len) = {
            let file_size = inner.size;
            let start = file_size.min(offset).align_down(sector_size);
            let end = file_size.min(offset + buf.len()).align_down(sector_size);
            (start, end - start)
        };

        inner
            .page_cache()
            .pages()
            .decommit(read_off..read_off + read_len)?;

        let mut buf_offset = 0;
        let frame = VmAllocOptions::new(1).uninit(true).alloc_single().unwrap();

        for bid in read_off / sector_size..(read_off + read_len) / sector_size {
            inner.read_sector(bid, &frame)?;
            frame.read_bytes(0, &mut buf[buf_offset..buf_offset + sector_size])?;
            buf_offset += sector_size;
        }
        Ok(read_len)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        //We need to obtain the lock to resize the file.
        {
            let mut inner = self.0.write();
            if inner.inode_type.is_directory() {
                return_errno!(Errno::EISDIR)
            }

            let file_size = inner.size;
            let new_size = offset + buf.len();

            if new_size > file_size {
                inner.lock_and_resize(new_size)?;
                inner.page_cache.pages().resize(new_size)?;

                inner.size = new_size;
                //TODO:We need to fill the page cache with 0.
            }
        }

        //Lock released here.

        self.0.read().page_cache.pages().write_bytes(offset, buf)?;

        Ok(buf.len())
    }

    fn write_direct_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut inner = self.0.write();
        if inner.inode_type.is_directory() {
            return_errno!(Errno::EISDIR)
        }
        if !is_block_aligned(offset) || !is_block_aligned(buf.len()) {
            return_errno_with_message!(Errno::EINVAL, "not block-aligned");
        }

        let file_size = inner.size;
        let end_offset = offset + buf.len();

        let start = offset.min(file_size);
        let end = end_offset.min(file_size);
        inner.page_cache.pages().decommit(start..end)?;

        if end_offset > file_size {
            inner.page_cache.pages().resize(end_offset)?;
            inner.lock_and_resize(end_offset)?;
            inner.size = end_offset;
        }

        //TODO: We need to write 0 to extented space.

        let block_size = inner.fs().sector_size();

        let mut buf_offset = 0;
        for bid in offset / block_size..(end_offset) / block_size {
            let frame = {
                let frame = VmAllocOptions::new(1).uninit(true).alloc_single().unwrap();
                frame.write_bytes(0, &buf[buf_offset..buf_offset + block_size])?;
                frame
            };
            inner.write_sector(bid, &frame)?;
            buf_offset += block_size;
        }

        Ok(buf_offset)
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        let mut inner = self.0.write();
        if !inner.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        if name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }

        let fs = inner.fs();
        let guard = fs.lock();

        if inner.lookup_by_name(name).is_ok() {
            return_errno!(Errno::EEXIST)
        }

        let result = inner.add_entry(name, type_, mode)?;
        let _ = fs.insert_inode(result.clone());
        Ok(result)
    }

    fn mknod(&self, name: &str, mode: InodeMode, dev: Arc<dyn Device>) -> Result<Arc<dyn Inode>> {
        return_errno_with_message!(Errno::EINVAL, "unsupported operation")
    }

    fn readdir_at(&self, dir_cnt: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        let inner = self.0.read();

        if dir_cnt >= inner.num_subdir as usize {
            return Ok(0);
        }

        let mut empty_visitor = EmptyVistor;

        let fs = inner.fs();
        let guard = fs.lock();

        //Skip previous directories.
        let off = inner.read_multiple_dirs(0, dir_cnt, &mut empty_visitor)?;

        inner.read_multiple_dirs(off, inner.num_subdir as usize - dir_cnt, visitor)?;

        Ok(inner.num_subdir as usize - dir_cnt)
    }

    fn link(&self, old: &Arc<dyn Inode>, name: &str) -> Result<()> {
        return_errno_with_message!(Errno::EINVAL, "unsupported operation")
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut inner = self.0.write();
        if !inner.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        if name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }
        if name == "." || name == ".." {
            return_errno!(Errno::EISDIR)
        }

        let fs = inner.fs();
        let guard = fs.lock();

        let (inode, offset, len) = inner.lookup_by_name(name)?;
        if inode.type_() != InodeType::File {
            return_errno!(Errno::EISDIR)
        }

        inode.0.write().resize(0)?;

        inner.delete_dentry_set(offset, len)?;
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        let mut inner = self.0.write();
        if !inner.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }
        if name == "." {
            return_errno_with_message!(Errno::EINVAL, "rmdir on .")
        }
        if name == ".." {
            return_errno_with_message!(Errno::ENOTEMPTY, "rmdir on ..")
        }
        if name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }

        let fs = inner.fs();
        let guard = fs.lock();

        let (inode, offset, len) = inner.lookup_by_name(name)?;
        if inode.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR)
        } else if !inode.0.read().is_empty_dir()? {
            // check if directory to be deleted is empty
            return_errno!(Errno::ENOTEMPTY)
        }

        inode.0.write().resize(0)?;

        inner.delete_dentry_set(offset, len)?;
        Ok(())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        //FIXME: Readdir should be immutable instead of mutable, but there will be no performance issues due to the global fs lock.
        let inner = self.0.read();
        if !inner.inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }

        if name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }

        let fs = inner.fs();
        let guard = fs.lock();

        let (inode, _, _) = inner.lookup_by_name(name)?;
        Ok(inode)
    }

    fn rename(&self, old_name: &str, target: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return_errno!(Errno::EISDIR);
        }
        if old_name.len() > MAX_NAME_LENGTH || new_name.len() > MAX_NAME_LENGTH {
            return_errno!(Errno::ENAMETOOLONG)
        }

        let Some(target_) = target.downcast_ref::<ExfatInode>() else {
            return_errno_with_message!(Errno::EINVAL, "not an exfat inode")
        };

        let fs = self.0.read().fs();
        let guard = fs.lock();

        //There will be no deadlocks since the whole fs is locked.

        if !self.0.read().inode_type.is_directory() || !target_.0.read().inode_type.is_directory() {
            return_errno!(Errno::ENOTDIR)
        }

        // rename something to itself, return success directly
        if self.0.read().ino == target_.0.read().ino && old_name.eq(new_name) {
            return Ok(());
        }

        // read 'old_name' file or dir and its dentries
        let (old_inode, old_offset, old_len) = self.0.read().lookup_by_name(old_name)?;

        let lookup_exist_result = target_.0.read().lookup_by_name(new_name);
        if let Ok((ref exist_inode, exist_offset, exist_len)) = lookup_exist_result {
            // check for two corner cases here
            // if 'old_name' represents a directory, the exist 'new_name' must represents a empty directory.
            if old_inode.0.write().inode_type.is_directory()
                && !exist_inode.0.read().is_empty_dir()?
            {
                return_errno!(Errno::ENOTEMPTY)
            }
            // if 'old_name' represents a file, the exist 'new_name' must also represents a file.
            if old_inode.0.write().inode_type.is_reguler_file()
                && exist_inode.0.read().inode_type.is_directory()
            {
                return_errno!(Errno::EISDIR)
            }
        }

        // copy the dentries
        let new_inode =
            target_
                .0
                .write()
                .add_entry(new_name, old_inode.type_(), old_inode.mode())?;

        // evict old_inode
        fs.evict_inode(old_inode.0.write().hash_index());
        // update metadata
        old_inode.0.write().copy_metadata_from(new_inode);
        // insert back
        let _ = fs.insert_inode(old_inode);

        // delete 'old_name' dentries
        self.0.write().delete_dentry_set(old_offset, old_len)?;

        // remove the exist 'new_name' file(include file contents, inode and dentries)
        if let Ok((exist_inode, exist_offset, exist_len)) = lookup_exist_result {
            fs.evict_inode(exist_inode.hash_index());
            exist_inode.0.write().resize(0)?;
            target_
                .0
                .write()
                .delete_dentry_set(exist_offset, exist_len)?;
        }

        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        return_errno_with_message!(Errno::EINVAL, "unsupported operation")
    }

    fn write_link(&self, target: &str) -> Result<()> {
        return_errno_with_message!(Errno::EINVAL, "unsupported operation")
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<i32> {
        todo!()
    }

    fn sync(&self) -> Result<()> {
        self.0.read().page_cache().evict_range(0..self.len())?;

        let mut inner = self.0.write();
        let fs = inner.fs();
        let guard = fs.lock();

        inner.write_inode(true)?;
        Ok(())
    }

    fn poll(&self, mask: IoEvents, _poller: Option<&Poller>) -> IoEvents {
        let events = IoEvents::IN | IoEvents::OUT;
        events & mask
    }

    fn is_dentry_cacheable(&self) -> bool {
        true
    }
}
