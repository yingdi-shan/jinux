//! The block devices of jinux
#![no_std]
#![forbid(unsafe_code)]
#![feature(fn_traits)]
#![feature(step_trait)]
#![feature(trait_upcasting)]
#![allow(dead_code)]

extern crate alloc;

pub mod bio;
pub mod id;
mod impl_block_device;
mod prelude;
pub mod request_queue;

use self::{prelude::*, request_queue::BioRequestQueue};

use component::init_component;
use component::ComponentInitError;
use jinux_frame::sync::SpinLock;

use spin::Once;

pub const BLOCK_SIZE: usize = jinux_frame::config::PAGE_SIZE;
pub const SECTOR_SIZE: usize = 512;

pub trait BlockDevice: Send + Sync + Any + Debug {
    /// Returns a `BioRequestQueue` for this device.
    fn request_queue(&self) -> &dyn BioRequestQueue;
    fn handle_irq(&self);
}

impl dyn BlockDevice {
    pub fn downcast_ref<T: BlockDevice>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref::<T>()
    }
}

pub fn register_device(name: String, device: Arc<dyn BlockDevice>) {
    COMPONENT
        .get()
        .unwrap()
        .block_device_table
        .lock()
        .insert(name, device);
}

pub fn get_device(str: &str) -> Option<Arc<dyn BlockDevice>> {
    COMPONENT
        .get()
        .unwrap()
        .block_device_table
        .lock()
        .get(str)
        .cloned()
}

pub fn all_devices() -> Vec<(String, Arc<dyn BlockDevice>)> {
    let block_devs = COMPONENT.get().unwrap().block_device_table.lock();
    block_devs
        .iter()
        .map(|(name, device)| (name.clone(), device.clone()))
        .collect()
}

static COMPONENT: Once<Component> = Once::new();

#[init_component]
fn component_init() -> Result<(), ComponentInitError> {
    let a = Component::init()?;
    COMPONENT.call_once(|| a);
    Ok(())
}

#[derive(Debug)]
struct Component {
    block_device_table: SpinLock<BTreeMap<String, Arc<dyn BlockDevice>>>,
}

impl Component {
    pub fn init() -> Result<Self, ComponentInitError> {
        Ok(Self {
            block_device_table: SpinLock::new(BTreeMap::new()),
        })
    }
}
