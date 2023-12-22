use crate::prelude::*;

use jinux_block::BlockDevice;
use jinux_virtio::device::block::DEVICE_NAME as VIRTIO_BLK_NAME;
use spin::Once;

static VIRTIO_BLK_DEVICE: Once<Arc<dyn BlockDevice>> = Once::new();

pub fn init() {
    VIRTIO_BLK_DEVICE.call_once(|| -> Arc<dyn BlockDevice> {
        jinux_block::get_device(VIRTIO_BLK_NAME).unwrap()
    });
}

pub fn virtio_blk_device() -> Arc<dyn BlockDevice> {
    VIRTIO_BLK_DEVICE.get().unwrap().clone()
}
