use crate::fs::exfat::fat::ExfatChain;

use crate::fs::exfat::fs::ExfatFS;
use crate::fs::utils::{Inode, InodeType, PageCache};

use super::constants::*;
use super::dentry::ExfatDentry;
use super::utils::{convert_dos_time_to_duration, le16_to_cpu};
use super::fat::{FatValue, FatTrait};
use crate::events::IoEvents;
use crate::fs::device::Device;
use crate::fs::utils::DirentVisitor;
use crate::fs::utils::InodeMode;
use crate::fs::utils::IoctlCmd;
use crate::prelude::*;
use crate::process::signal::Poller;
use crate::vm::vmo::Vmo;
use alloc::string::String;
use core::time::Duration;
use jinux_frame::vm::{VmFrame, VmIo};
use jinux_rights::Full;

#[derive(Default, Debug)]
pub struct ExfatDentryName(Vec<u16>);

impl ExfatDentryName {
    pub fn push(&mut self, value: u16) {
        self.0.push(value);
    }

    pub fn from(name: &str) -> Self {
        ExfatDentryName {
            0: name.encode_utf16().collect(),
        }
    }

    pub fn new() -> Self {
        ExfatDentryName { 0: Vec::new() }
    }

    pub fn is_name_valid(&self) -> bool{
        //TODO:verify the name
        true

    }
}

impl ToString for ExfatDentryName {
    fn to_string(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
}

//In-memory rust object that represents a file or folder.
#[derive(Debug)]
pub struct ExfatInode {
    parent_dir: ExfatChain,
    parent_entry: u32,

    type_: u16,

    attr: u16,
    start_cluster: u32,

    flags: u8,
    version: u32,

    //Logical size
    size: usize,
    //Allocated size
    capacity: usize,

    //Usefor for folders
    //num_subdirs: u32,

    atime: Duration,
    mtime: Duration,
    ctime: Duration,

    //exFAT uses UTF-16 encoding, rust use utf-8 for string processing.
    name: ExfatDentryName,

    page_cache:PageCache,

    fs: Weak<ExfatFS>
}

impl ExfatInode {

    pub(super) fn new() -> Arc<Self> {
        let inode = Arc::new_cyclic(|weak_self| Self {
            parent_dir: ExfatChain::default(),
            parent_entry: 0,
            type_: 0,
            attr: 0,
            size:0,
            start_cluster:0,
            flags: 0,
            version: 0,
            capacity: 0,
            atime:Duration::default(),
            mtime:Duration::default(),
            ctime:Duration::default(),
            name:ExfatDentryName::default(),
            fs: Weak::default(),
            page_cache:PageCache::new(weak_self.clone() as _).unwrap()
        });
        inode
    }
    pub fn fs(&self) -> Arc<ExfatFS> {
        self.fs.upgrade().unwrap()
    }


    fn read_inode(fs: Arc<ExfatFS>, p_dir: ExfatChain, entry: u32) -> Result<Arc<Self>> {
        let dentry = fs.get_dentry(&p_dir, entry)?;
        if let ExfatDentry::File(file) = dentry {

            let type_ = if (file.attribute & ATTR_SUBDIR) != 0 {
                TYPE_DIR
            } else {
                TYPE_FILE
            };

            let ctime = convert_dos_time_to_duration(
                file.create_tz,
                file.create_date,
                file.create_time,
                file.create_time_cs,
            )?;
            let mtime = convert_dos_time_to_duration(
                file.modify_tz,
                file.modify_date,
                file.modify_time,
                file.modify_time_cs,
            )?;
            let atime = convert_dos_time_to_duration(
                file.access_tz,
                file.access_date,
                file.access_time,
                0,
            )?;

            let dentry_set = fs.get_dentry_set(&p_dir, entry, ES_ALL_ENTRIES)?;

            if dentry_set.len() < EXFAT_FILE_MIMIMUM_DENTRY {
                return_errno_with_message!(Errno::EINVAL, "Invalid dentry length")
            }

            //STREAM Dentry must immediately follows file dentry
            if let ExfatDentry::Stream(stream) = dentry_set[1] {
                let size = stream.valid_size as usize;
                let start_cluster = stream.start_cluster;
                let flags = stream.flags;
                //Read name from dentry
                let name = Self::read_name_from_dentry_set(&dentry_set);
                if !name.is_name_valid() {
                    return_errno_with_message!(Errno::EINVAL, "Invalid name")
                }

                let fs_weak = Arc::downgrade(&fs);

                return Ok(Arc::new_cyclic(|weak_self|ExfatInode {
                    parent_dir: p_dir,
                    parent_entry: entry,
                    type_: type_,
                    attr: file.attribute,
                    size,
                    start_cluster,
                    flags: flags,
                    version: 0,
                    capacity: 0,
                    atime,
                    mtime,
                    ctime,
                    name,
                    fs: fs_weak,
                    page_cache:PageCache::new(weak_self.clone() as _).unwrap()
                }));
            } else {
                return_errno_with_message!(Errno::EINVAL, "Invalid stream dentry")
            }
        }
        return_errno_with_message!(Errno::EINVAL,"Invalid file dentry")
    }

    fn read_name_from_dentry_set(dentry_set: &[ExfatDentry]) -> ExfatDentryName {
        let mut name: ExfatDentryName = ExfatDentryName::new();
        for i in 2..dentry_set.len() {
            if let ExfatDentry::Name(name_dentry) = dentry_set[i] {
                for character in name_dentry.unicode_0_14 {
                    if character == 0 {
                        return name;
                    } else {
                        name.push(le16_to_cpu(character));
                    }
                }
            } else {
                //End of name dentry
                break;
            }
        }
        name
    }
    // if new_size > capacity, alloc more; else just set a new size
    // 1: capacity = 0?  how to find the first cluster if no_fat_chain is set?(how to make sure big enough)
    // 2: resize need to modify the inode struct?(&mut self)
    // 3: when new_size < capacity, should we free some allocated but not in use clusters?
    pub fn resize(&self,new_size:usize) -> Result<()> {
        if new_size <= self.capacity {
            //self.size = new_size;
            return Ok(());
        }
        // need to alloc new clusters
        let no_fat_chain: bool = self.flags == ALLOC_POSSIBLE|ALLOC_NO_FAT_CHAIN;
        let fs = self.fs.upgrade().unwrap();
        let bitmap = &fs.bitmap;
        let sb = &fs.super_block;
        //if new_size > self.size {
            let cur_cluster_num: u32 = (self.size as u32 + sb.cluster_size - 1) >> sb.cluster_size_bits;
            let need_cluster_num: u32 = (new_size as u32 + sb.cluster_size - 1) >> sb.cluster_size_bits;
            let alloc_num = need_cluster_num - cur_cluster_num;
            // allocate new clusters
            if no_fat_chain {
                // no fat used, only modify bitmap
                let cur_end_cluster = self.start_cluster + cur_cluster_num;
                for i in 0..alloc_num {
                    //bitmap.set_bitmap_used(cur_end_cluster + i as u32, true)?;
                }
            }
            else {
                let mut edit_fat_cluster = self.start_cluster;
                for i in 0..(cur_cluster_num - 1) {
                    match fs.get_next_fat(edit_fat_cluster)? {
                        FatValue::Data(entry) => { edit_fat_cluster = entry; }
                        _ => return_errno_with_message!(Errno::EINVAL, "Invalid fat entry")
                    }
                }
                for i in 0..alloc_num {
                    let free_cluster = bitmap.find_free_bitmap(EXFAT_FIRST_CLUSTER)?;
                    //bitmap.set_bitmap_used(free_cluster, true)?;
                    fs.set_next_fat(edit_fat_cluster, FatValue::Data(free_cluster))?;
                    edit_fat_cluster = free_cluster;
                }
                fs.set_next_fat(edit_fat_cluster, FatValue::EndOfChain)?;
            }
            //self.capacity = (need_cluster_num << sb.cluster_size_bits) as usize;
            //self.size = new_size;
        //}
        /* 
        else if new_size < self.size {
            let trunc_size = self.size - new_size;
            // truncate exist clusters
            if no_fat_chain {
                let new_end_cluster = self.start_cluster + new_size as u32;
                for i in 0..trunc_size {
                    bitmap.set_bitmap_unused(new_end_cluster + i as u32, true)?;
                }
            }
            else {
                let mut edit_fat_cluster = self.start_cluster;
                for i in 0..(new_size - 1) {
                    match fs.get_next_fat(edit_fat_cluster)? {
                        FatValue::Data(entry) => { edit_fat_cluster = entry; }
                        _ => return_errno_with_message!(Errno::EINVAL, "Invalid fat entry")
                    }
                }
                let mut new_end_cluster = edit_fat_cluster;
                for i in 0..trunc_size {
                    let next_cluster: u32;
                    match fs.get_next_fat(new_end_cluster)? {
                        FatValue::Data(entry) => { next_cluster = entry; }
                        _ => return_errno_with_message!(Errno::EINVAL, "Invalid fat entry")
                    }
                    bitmap.set_bitmap_unused(next_cluster, true)?;
                    fs.set_next_fat(new_end_cluster, FatValue::Free)?;
                    new_end_cluster = next_cluster;
                }
                fs.set_next_fat(new_end_cluster, FatValue::EndOfChain)?;
            }
            self.size = new_size;
        }
        */
        Ok(())
    }

    pub fn page_cache(&self) -> &PageCache {
        &self.page_cache
    }

    
}

impl Inode for ExfatInode {
    fn len(&self) -> usize {
        self.size as usize
    }

    fn resize(&self, new_size: usize) {
        todo!()
    }

    fn metadata(&self) -> crate::fs::utils::Metadata {
        todo!()
    }

    fn type_(&self) -> crate::fs::utils::InodeType {
        match self.type_{
            TYPE_FILE => InodeType::File,
            TYPE_DIR => InodeType::Dir,
            _ => todo!()
        }
    }

    fn mode(&self) -> crate::fs::utils::InodeMode {
        todo!()
    }

    fn set_mode(&self, mode: crate::fs::utils::InodeMode) {
        todo!()
    }

    fn atime(&self) -> core::time::Duration {
        self.atime
    }

    fn set_atime(&self, time: core::time::Duration) {
        todo!()
    }

    fn mtime(&self) -> core::time::Duration {
        self.mtime
    }

    fn set_mtime(&self, time: core::time::Duration) {
        todo!()
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::fs::utils::FileSystem> {
        self.fs()
    }

    fn read_page(&self, idx: usize, frame: &VmFrame) -> Result<()> {
        todo!()
    }

    fn write_page(&self, idx: usize, frame: &VmFrame) -> Result<()> {
        todo!()
    }


    fn page_cache(&self) -> Option<Vmo<Full>> {
        Some(self.page_cache().pages().dup().unwrap())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        if self.type_() != InodeType::File {
            return_errno!(Errno::EISDIR)
        }
        let (off,read_len) = {
            let file_size = self.len();
            let start = file_size.min(offset);
            let end = file_size.min(offset + buf.len());
            (start,end - start)
        };
        self.page_cache.pages().read_bytes(offset, &mut buf[..read_len])?;

        Ok(read_len)
    }

    fn read_direct_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        todo!()
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        if self.type_() != InodeType::File {
            return_errno!(Errno::EISDIR)
        }

        let file_size = self.len();
        let new_size = offset + buf.len();
        if new_size > file_size {
            self.page_cache.pages().resize(new_size)?;
        }
        self.page_cache.pages().write_bytes(offset, buf)?;
        if new_size > file_size {
            self.resize(new_size)?;
        }

        Ok(buf.len())
    }

    fn write_direct_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        todo!()
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        todo!()
    }

    fn mknod(&self, name: &str, mode: InodeMode, dev: Arc<dyn Device>) -> Result<Arc<dyn Inode>> {
        todo!()
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        todo!()
    }

    fn link(&self, old: &Arc<dyn Inode>, name: &str) -> Result<()> {
        todo!()
    }

    fn unlink(&self, name: &str) -> Result<()> {
        todo!()
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        todo!()
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        todo!()
    }

    fn rename(&self, old_name: &str, target: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        todo!()
    }

    fn read_link(&self) -> Result<String> {
        todo!()
    }

    fn write_link(&self, target: &str) -> Result<()> {
        todo!()
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<i32> {
        todo!()
    }

    fn sync(&self) -> Result<()> {
        todo!()
    }

    fn poll(&self, mask: IoEvents, _poller: Option<&Poller>) -> IoEvents {
        let events = IoEvents::IN | IoEvents::OUT;
        events & mask
    }

    /// Returns whether a VFS dentry for this inode should be put into the dentry cache.
    ///
    /// The dentry cache in the VFS layer can accelerate the lookup of inodes. So usually,
    /// it is preferable to use the dentry cache. And thus, the default return value of this method
    /// is `true`.
    ///
    /// But this caching can raise consistency issues in certain use cases. Specifically, the dentry
    /// cache works on the assumption that all FS operations go through the dentry layer first.
    /// This is why the dentry cache can reflect the up-to-date FS state. Yet, this assumption
    /// may be broken. If the inodes of a file system may "disappear" without unlinking through the
    /// VFS layer, then their dentries should not be cached. For example, an inode in procfs
    /// (say, `/proc/1/fd/2`) can "disappear" without notice from the perspective of the dentry cache.
    /// So for such inodes, they are incompatible with the dentry cache. And this method returns `false`.
    ///
    /// Note that if any ancestor directory of an inode has this method returns `false`, then
    /// this inode would not be cached by the dentry cache, even when the method of this
    /// inode returns `true`.
    fn is_dentry_cacheable(&self) -> bool {
        true
    }
}
