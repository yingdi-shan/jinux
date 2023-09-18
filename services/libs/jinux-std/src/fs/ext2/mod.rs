//! A safe Rust Ext2 filesystem.
//!
//! Here we summarizes the features that need to be implemented in the future.
//! 1. Supports large file.
//! 2. Supports merging small read/write operations.
//! 3. Handles the intermediate failure status correctly.

pub use fs::Ext2;
pub use inode::{FilePerm, FileType, Inode};
pub use super_block::{SuperBlock, MAGIC_NUM};

mod block_group;
mod blocks_hole;
mod dir;
mod fs;
mod impl_for_vfs;
mod inode;
mod prelude;
mod super_block;
mod utils;
