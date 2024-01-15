pub mod device;
pub mod devpts;
pub mod epoll;
pub mod exfat;
pub mod ext2;
pub mod file_handle;
pub mod file_table;
pub mod fs_resolver;
pub mod inode_handle;
pub mod pipe;
pub mod procfs;
pub mod ramfs;
pub mod rootfs;
pub mod utils;

use crate::fs::exfat::{ExfatFS, ExfatMountOptions};
use crate::fs::fs_resolver::FsPath;
use crate::prelude::*;
use crate::thread::kernel_thread::KernelThreadExt;
use jinux_virtio::device::block::device::BlockDevice as VirtIoBlkDevice;

pub fn lazy_init() {
    let block_device = crate::driver::block::virtio_blk_device();
    let cloned_block_device = block_device.clone();

    let task_fn = move || {
        info!("spawn the virt-io-blk thread");
        let virtio_blk_device = block_device.downcast_ref::<VirtIoBlkDevice>().unwrap();
        loop {
            virtio_blk_device.handle_requests();
        }
    };
    crate::Thread::spawn_kernel_thread(crate::ThreadOptions::new(task_fn));

    let exfat_fs = ExfatFS::open(cloned_block_device, ExfatMountOptions::default());
    if exfat_fs.is_err() {
        error!("{:?}", exfat_fs.unwrap_err())
    } else {
        let target_path = FsPath::try_from("/exfat").unwrap();
        println!("[kernel] Mount Exfat fs at {:?} ", target_path);
        self::rootfs::mount_fs_at(exfat_fs.unwrap(), &target_path).unwrap();
    }

    // let ext2_fs = Ext2::open(cloned_block_device).unwrap();
    // let target_path = FsPath::try_from("/ext2").unwrap();
    // println!("[kernel] Mount Ext2 fs at {:?} ", target_path);
    // self::rootfs::mount_fs_at(ext2_fs, &target_path).unwrap();
}
