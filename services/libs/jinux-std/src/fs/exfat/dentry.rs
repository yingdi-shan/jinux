use jinux_frame::vm::VmIo;

use crate::fs::exfat::constants::*;
use crate::prelude::*;

use super::fat::FatTrait;
use super::fs:: ExfatFS;
use super::fat::ExfatChain;

use crate::fs::exfat::fat::FatValue;

pub enum ExfatDentry {
    File(ExfatFileDentry),
    Stream(ExfatStreamDentry),
    Name(ExfatNameDentry),
    Bitmap(ExfatBitmapDentry),
    Upcase(ExfatUpcaseDentry),
    VendorExt(ExfatVendorExtDentry),
    VendorAlloc(ExfatVendorAllocDentry),
    GenericSecondary(ExfatGenericSecondaryDentry),
    Deleted,
    UnUsed
}

impl TryFrom<&[u8]> for ExfatDentry {
    type Error = crate::error::Error;
    fn try_from(value: &[u8]) -> Result<Self> {
        if value.len() != DENTRY_SIZE {
            return_errno_with_message!(Errno::EINVAL,"directory entry size mismatch.")
        }
        match value[0] {
            EXFAT_FILE => Ok(ExfatDentry::File(ExfatFileDentry::from_bytes(value))),
            EXFAT_STREAM => Ok(ExfatDentry::Stream(ExfatStreamDentry::from_bytes(value))),
            EXFAT_NAME => Ok(ExfatDentry::Name(ExfatNameDentry::from_bytes(value))),
            EXFAT_BITMAP => Ok(ExfatDentry::Bitmap(ExfatBitmapDentry::from_bytes(value))),
            EXFAT_UPCASE => Ok(ExfatDentry::Upcase(ExfatUpcaseDentry::from_bytes(value))),
            EXFAT_VENDOR_EXT => Ok(ExfatDentry::VendorExt(ExfatVendorExtDentry::from_bytes(value))),
            EXFAT_VENDOR_ALLOC => Ok(ExfatDentry::VendorAlloc(ExfatVendorAllocDentry::from_bytes(value))),
            
            EXFAT_UNUSED => Ok(ExfatDentry::UnUsed),
            x if IS_EXFAT_DELETED(x) => Ok(ExfatDentry::Deleted),
            _ => return_errno_with_message!(Errno::EINVAL,"unrecognized dentry type")
        }
        
    }
}


pub enum ExfatValidateDentryMode {
	Started,
	GetFileEntry,
	GetStreamEntry,
	GetNameEntry,
	GetCriticalSecEntry,
	GetBenignSecEntry,
}

pub struct ExfatDentryIterator{
    fs: Weak<ExfatFS>,
    entry: u32,
    chain: ExfatChain,
    has_error : bool
}

impl ExfatDentryIterator {
    pub fn from(fs: Weak<ExfatFS>,entry: u32, chain: ExfatChain) -> Self {
            Self{
                fs,
                entry,
                chain,
                has_error:false
            }
        }
}

impl Iterator for ExfatDentryIterator {
    type Item = Result<ExfatDentry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.chain.dir == EXFAT_EOF_CLUSTER || self.has_error{
            None
        } else {
            let dentry_result = self.fs.upgrade().unwrap().get_dentry(&self.chain, self.entry);

            //Should stop iterating if the result is Err. 
            if dentry_result.is_err() {
                self.has_error = true;
                return Some(dentry_result);
            }

            //Stop iterating if the dentry is unused
            match dentry_result.unwrap() {
                ExfatDentry::UnUsed => None,
                dentry => {
                    // Instead of calling get_dentry directly, update the chain and entry of the iterator to reduce the read of FAT table. 
                    if self.entry + 1 == self.fs.upgrade().unwrap().super_block().dentries_per_clu {
                        self.entry = 0;
                        let next_fat = self.fs.upgrade().unwrap().get_next_fat(self.chain.dir);
                        if next_fat.is_err() {
                            self.has_error = true;
                            return Some(Result::Err(next_fat.unwrap_err()));
                        }
                        self.chain.dir = next_fat.unwrap().into()
                    } else {
                        self.entry += 1;
                    }
                    Some(Ok(dentry))
                }
            }
        }
    }
}

// pub trait ExfatDentryReader {
//     fn get_dentry_set(&self,parent_dir:&ExfatChain,entry:u32,_type:u32) -> Result<&[ExfatDentry]>;
//     //Get the i th dentry in the parent_dir cluster.
//     fn get_dentry(&self,parent_dir:&ExfatChain,entry:u32) -> Result<ExfatDentry>;
// }

/// Implement dentry related functions.
impl ExfatFS{
    /// Returns a set of dentries for a file or dir.
    /// chain+entry:  indicates a file/dir
    /// type:  specifies how many dentries should be included.
    pub fn get_dentry_set(&self,parent_dir:&ExfatChain,entry:u32,_type:u32) -> Result<Vec<ExfatDentry>> {
        let primary_dentry = self.get_dentry(parent_dir, entry)?;

        let mut status = ExfatValidateDentryMode::Started;
        if let ExfatDentry::File(file_dentry) = primary_dentry {

            let num_entries : usize = if _type == ES_ALL_ENTRIES {
                file_dentry.num_secondary as usize + 1
            } else {
                _type as usize
            };

            let mut dentries = Vec::<ExfatDentry>::with_capacity(num_entries);
            dentries.push(primary_dentry);

            //TODO: Should use bulked read for improved performance. 
            for i in 1..num_entries{
                let dentry = self.get_dentry(parent_dir, entry + i as u32)?;
                status = self.validate_dentry(&dentry,status)?;
                dentries.push(dentry);
            }
            
            Ok(dentries)
        } else {
            return_errno_with_message!(Errno::EIO,"invalid dentry type")
        }

    }

    //TODO: Validate the status of dentry by using a state machine.
    fn validate_dentry(&self,dentry:&ExfatDentry, status:ExfatValidateDentryMode) -> Result<ExfatValidateDentryMode>{
        unimplemented!();
    }

    pub fn get_dentry(&self,parent_dir:&ExfatChain,entry:u32) -> Result<ExfatDentry> {
        if parent_dir.dir == DIR_DELETED {
            return_errno_with_message!(Errno::EIO,"access to deleted dentry")
        }

        let dentry_offset = self.find_dentry_location(parent_dir, entry)?;

        //TODO: read ahead until the next page to improve the performance of the dentry.

        let mut buf:[u8;DENTRY_SIZE] = [0;DENTRY_SIZE];

        //FIXME: Should I maintain a page cache for the whole filesystem?
        self.block_device().read_bytes(dentry_offset, & mut buf)?;

        ExfatDentry::try_from(buf.as_bytes())
    }


    ///return the offset of the specified entry.
    fn find_dentry_location(&self,parent_dir:&ExfatChain,entry:u32) -> Result<usize> {
        let off = (entry as usize) * (DENTRY_SIZE);
        let mut cur_cluster = parent_dir.dir;
        let mut cluster_offset : u32 = (off >> self.super_block().cluster_size_bits).try_into().unwrap();
        if parent_dir.flags == ALLOC_NO_FAT_CHAIN {
            cur_cluster += cluster_offset;
        } else {
            // The target cluster should be in the {cluster_offset}th cluster.
            while cluster_offset > 0 {
                
                let fat = self.get_next_fat(cur_cluster)?;
                match fat {
                    FatValue::Data(value) => cur_cluster = value,
                    _ => return_errno_with_message!(Errno::EIO,"Invalid dentry access beyond EOF")
                };
                cluster_offset-=1;
            }
        }
        Ok(((cur_cluster as usize) << self.super_block().cluster_size_bits) + (off % self.super_block().cluster_size as usize))
    }


    
}



#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatFileDentry {
    pub(super) dentry_type: u8,
    //Number of Secondary directory entries.
    pub(super) num_secondary: u8,
     //Calculated on file and secondary entries.
    pub(super) checksum: u16,

    pub(super) attribute: u16,
    pub(super) reserved1: u16,

    pub(super) create_time: u16,
    pub(super) create_date: u16,

    pub(super) modify_time: u16,
    pub(super) modify_date: u16,

    pub(super) access_time: u16,
    pub(super) access_date: u16,

    pub(super) create_time_cs: u8,
    pub(super) modify_time_cs: u8,

    pub(super) create_tz: u8,
    pub(super) modify_tz: u8,
    pub(super) access_tz: u8,

    pub(super) reserved2: [u8; 7],
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatStreamDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) reserved1: u8,
    pub(super) name_len: u8,
    pub(super) name_hash: u16,
    pub(super) reserved2: u16,
    pub(super) valid_size: u64,
    pub(super) reserved3: u32,
    pub(super) start_cluster: u32,
    pub(super) size: u64,
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatNameDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) unicode_0_14: [u16; EXFAT_FILE_NAME_LEN],
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatBitmapDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) reserved: [u8; 18],
    pub(super) start_cluster: u32,
    pub(super) size: u64,
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatUpcaseDentry {
    pub(super) dentry_type: u8,
    pub(super) reserved1: [u8; 3],
    pub(super) checksum: u32,
    pub(super) reserved2: [u8; 12],
    pub(super) start_cluster: u32,
    pub(super) size: u64,
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatVendorExtDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) vendor_guid: [u8; 16],
    pub(super) vendor_defined: [u8; 14],
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatVendorAllocDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) vendor_guid: [u8; 16],
    pub(super) vendor_defined: [u8; 2],
    pub(super) start_cluster: u32,
    pub(super) size: u64,
}

#[repr(C, packed)]
#[derive(Clone,Debug,Default,Copy,Pod)]
pub struct ExfatGenericSecondaryDentry {
    pub(super) dentry_type: u8,
    pub(super) flags: u8,
    pub(super) custom_defined: [u8; 18],
    pub(super) start_cluster: u32,
    pub(super) size: u64,
}

