use core::mem::size_of;
use jinux_frame::vm::VmIo;

use super::fs::ExfatFS;
use crate::prelude::*;
use super::super_block::ExfatSuperBlock;

pub type ClusterID = u32;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum FatValue {
    Free,
    Next(ClusterID),
    Bad,
    EndOfChain,
}

const EXFAT_EOF_CLUSTER: u32 = 0xFFFFFFFF;
const EXFAT_BAD_CLUSTER: u32 = 0xFFFFFFF7;
const EXFAT_FREE_CLUSTER: u32 = 0;
const FAT_ENTRY_SIZE:u32 = size_of::<ClusterID>();

impl From<ClusterID> for FatValue{
    fn from(value:ClusterID) -> Self{
        match value{
            EXFAT_BAD_CLUSTER => FatValue::Bad,
            EXFAT_FREE_CLUSTER => FatValue::Free,
            EXFAT_EOF_CLUSTER => FatValue::EndOfChain,
            _ => FatValue::Next(value)
        }
    }
}

impl From<FatValue> for ClusterID {
    fn from(val: FatValue) -> Self {
        match val{
            FatValue::Free => EXFAT_FREE_CLUSTER,
            FatValue::EndOfChain => EXFAT_EOF_CLUSTER,
            FatValue::Bad => EXFAT_BAD_CLUSTER,
            FatValue::Next(x) => x
        }
    }
}


//FIXME: Should we implement fat as a trait of file system, or as a member of file system?
pub trait FatTrait{
    fn read_next_fat(&self,cluster:ClusterID) -> Result<FatValue>;
    fn write_next_fat(&self,cluster:ClusterID,value:FatValue) -> Result<()>;
}

impl FatTrait for ExfatFS {
    
    fn read_next_fat(&self,cluster:ClusterID) -> Result<FatValue>{
        let sb : ExfatSuperBlock = self.super_block();
        let sector_size = sb.sector_size;

        if !self.is_valid_cluster(cluster) {
            return_errno_with_message!(Errno::EIO,"invalid access to FAT")
        }
        
        let position = sb.fat1_start_sector * sector_size as u64 + (cluster as u64) * FAT_ENTRY_SIZE as u64;
        let mut buf : [u8;FAT_ENTRY_SIZE] =  [0;FAT_ENTRY_SIZE];
        self.block_device().read_at(position as usize, &mut buf)?;

        let value = u32::from_le_bytes(buf);
        Ok(FatValue::from(value))
    }

    fn write_next_fat(&self,cluster:ClusterID,value:FatValue) -> Result<()> {
        let sb : ExfatSuperBlock = self.super_block();
        let sector_size = sb.sector_size;

        let position = sb.fat1_start_sector * sector_size as u64 + (cluster as u64) * FAT_ENTRY_SIZE as u64;
        let raw_value: u32 = value.into();
        
        //TODO: should make sure that the write is synchronous.
        self.block_device().write_at(position as usize, &raw_value.to_le_bytes())?;
        
        if sb.fat1_start_sector != sb.fat2_start_sector {
            let mirror_position = sb.fat2_start_sector * sector_size as u64 + (cluster as u64) * FAT_ENTRY_SIZE as u64;
            self.block_device().write_at(mirror_position as usize, &raw_value.to_le_bytes())?;
        }

       Ok(())
    }

}

bitflags! {
    pub struct FatChainFlags:u16 {
        //An associated allocation of clusters is possible
        const ALLOC_POSSIBLE = 0x01;
        //The allocated clusters are contiguous and fat table is irrevalent.
        const FAT_CHAIN_NOT_IN_USE = 0x03;
    }
}

// Directory pub structures 
#[derive(Default,Debug,Clone)]
pub struct ExfatChain {
    // current clusterID
    current: ClusterID,
    // use FAT or not
    flags: FatChainFlags,

    fs: Weak<ExfatFS>
}

//A position by the chain and relative offset in the cluster.
pub type ExfatChainPosition = (ExfatChain,usize);

impl ExfatChain {
    
    pub fn new(fs:Weak<ExfatFS>, current:ClusterID,flags:FatChainFlags) -> Self {
        Self { current, flags, fs }
    }

    fn fs(&self) -> Arc<ExfatFS>{
        self.fs.upgrade().unwrap()
    }

    //Walk to the cluster at the given offset, return the new relative offset
    pub fn walk_to_cluster_at_offset(&self,offset:usize) -> Result<ExfatChainPosition> {
        let cluster_size = self.fs().cluster_size();
        let steps = offset / cluster_size;
        let result_chain = self.walk(steps)?;
        let result_offset = offset % cluster_size;
        Ok(ExfatChainPosition(result_chain,result_offset))
    }

    pub fn walk(&self,steps:u32) -> Result<ExfatChain>{
        let result_cluster = self.current;
        if self.flags.contains(FatChainFlags::FAT_CHAIN_NOT_IN_USE) {
            result_cluster = (result_cluster + steps) as ClusterID;
        } else {
            for _ in 0..steps {
                let fat = self.fs().read_next_fat(result_cluster)?;
                match fat{
                    Next(next_fat) => result_cluster = next_fat,
                    _ => return_errno_with_message!(Errno::EIO,"invalid access to FAT cluster")
                }
            }
        }
        Ok(ExfatChain::new(self.fs.clone(),result_cluster,self.flags))
    }
}