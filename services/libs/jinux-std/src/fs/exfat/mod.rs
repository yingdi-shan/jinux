mod bitmap;
mod block_device;
mod constants;
mod dentry;
mod fat;
mod fs;
mod inode;
mod super_block;
mod upcase_table;
mod utils;

pub use fs::ExfatFS;
pub use inode::ExfatInode;

static EXFAT_IMAGE: &[u8] = include_bytes!("../../../../../../exfat.img");

use crate::fs::exfat::{block_device::ExfatMemoryDisk, fs::ExfatMountOptions};
use crate::prelude::*;
use alloc::boxed::Box;
use jinux_frame::vm::{VmAllocOptions, VmIo, VmSegment};

fn new_vm_segment_from_image() -> Result<VmSegment> {
    let vm_segment = VmAllocOptions::new(EXFAT_IMAGE.len() / PAGE_SIZE)
        .is_contiguous(true)
        .alloc_contiguous()?;

    vm_segment.write_bytes(0, EXFAT_IMAGE)?;
    Ok(vm_segment)
}

pub fn load_exfat() -> Arc<ExfatFS> {
    let vm_segment = new_vm_segment_from_image().unwrap();

    let disk = ExfatMemoryDisk::new(vm_segment);
    let mount_option = ExfatMountOptions::default();

    let fs = ExfatFS::open(Box::new(disk), mount_option);

    assert!(fs.is_ok(), "Fs failed to init:{:?}", fs.unwrap_err());

    fs.unwrap()
}

mod test {
    use crate::{
        device::Random,
        fs::{
            exfat::bitmap::EXFAT_RESERVED_CLUSTERS,
            exfat::block_device::SECTOR_SIZE,
            exfat::constants::MAX_NAME_LENGTH,
            utils::{Inode, InodeMode, InodeType},
        },
        prelude::*,
    };
    use alloc::sync::Arc;
    use hashbrown::HashMap;

    struct FileInMemory {
        pub name: String,
        pub inode: Arc<dyn Inode>,
        pub valid_len: usize,
        pub contents: Vec<u8>,
    }
    struct DirInMemory {
        pub depth: u32,
        pub name: String,
        pub inode: Arc<dyn Inode>,
        pub sub_names: Vec<String>,
        pub sub_dirs: HashMap<String, DentryInMemory>,
    }
    enum DentryInMemory {
        File(FileInMemory),
        Dir(DirInMemory),
    }
    enum Operation {
        Read(usize, usize),
        Write(usize, usize),
        Resize(usize),
        Create(String, InodeType),
        Lookup(String),
        Readdir(),
        Unlink(String),
        Rmdir(String),
        Rename(String, String),
    }

    impl DirInMemory {
        pub fn remove_by_name(&mut self, name: &String) {
            let _ = self.sub_dirs.remove(name);
            for idx in 0..self.sub_names.len() {
                if self.sub_names[idx].eq(name) {
                    self.sub_names.remove(idx);
                    break;
                }
            }
        }
        pub fn execute_and_test(&mut self, op: Operation, print: bool) {
            match op {
                Operation::Create(name, type_) => {
                    if print {
                        error!(
                            "Create: parent = {:?}, name = {:?}, type = {:?}",
                            self.name, name, type_
                        );
                    }

                    let create_result = self.inode.create(&name, type_, InodeMode::all());
                    if self.sub_dirs.contains_key(&name) {
                        assert!(create_result.is_err());
                        return;
                    }

                    assert!(
                        create_result.is_ok(),
                        "Fail to create {:?}: {:?}",
                        name,
                        create_result.unwrap_err()
                    );

                    let new_dentry_in_mem = if type_ == InodeType::File {
                        let file = FileInMemory {
                            name: name.clone(),
                            inode: create_result.unwrap(),
                            valid_len: 0,
                            contents: Vec::<u8>::new(),
                        };
                        DentryInMemory::File(file)
                    } else {
                        DentryInMemory::Dir(DirInMemory {
                            depth: self.depth + 1,
                            name: name.clone(),
                            inode: create_result.unwrap(),
                            sub_names: Vec::new(),
                            sub_dirs: HashMap::new(),
                        })
                    };
                    let _ = self.sub_dirs.insert(name.to_string(), new_dentry_in_mem);
                    self.sub_names.push(name.to_string());
                }
                Operation::Lookup(name) => {
                    if print {
                        error!("Lookup: parent = {:?}, name = {:?}", self.name, name);
                    }
                    let lookup_result = self.inode.lookup(&name);
                    if self.sub_dirs.get(&name).is_some() {
                        assert!(
                            lookup_result.is_ok(),
                            "Fail to lookup {:?}: {:?}",
                            name,
                            lookup_result.unwrap_err()
                        );
                    } else {
                        assert!(lookup_result.is_err());
                    }
                }
                Operation::Readdir() => {
                    if print {
                        error!("Readdir: parent = {:?}", self.name);
                    }
                    let mut sub: Vec<String> = Vec::new();
                    let readdir_result = self.inode.readdir_at(0, &mut sub);
                    assert!(readdir_result.is_ok(), "Fail to read directory",);
                    assert!(readdir_result.unwrap() == self.sub_dirs.len());
                    assert!(sub.len() == self.sub_dirs.len());

                    sub.sort();
                    self.sub_names.sort();

                    for i in 0..sub.len() {
                        assert!(
                            sub[i].eq(&self.sub_names[i]),
                            "Directory entry mismatch: read {:?} should be {:?}",
                            sub[i],
                            self.sub_names[i]
                        );
                    }
                }
                Operation::Unlink(name) => {
                    if print {
                        error!("Unlink: parent = {:?}, name = {:?}", self.name, name);
                    }
                    let unlink_result = self.inode.unlink(&name);
                    if let Option::Some(sub) = self.sub_dirs.get(&name)
                        && let DentryInMemory::File(_) = sub {
                        assert!(
                            unlink_result.is_ok(),
                            "Fail to remove file {:?}/{:?}: {:?}",
                            self.name, name, unlink_result.unwrap_err()
                        );
                        self.remove_by_name(&name);
                    }
                    else {
                        assert!(unlink_result.is_err());
                    }
                }
                Operation::Rmdir(name) => {
                    if print {
                        error!("Rmdir: parent = {:?}, name = {:?}", self.name, name);
                    }
                    let rmdir_result = self.inode.rmdir(&name);
                    if let Option::Some(sub) = self.sub_dirs.get(&name)
                        && let DentryInMemory::Dir(sub_dir) = sub
                        && sub_dir.sub_dirs.is_empty() {
                        assert!(
                            rmdir_result.is_ok(),
                            "Fail to remove directory {:?}/{:?}: {:?}",
                            self.name, name, rmdir_result.unwrap_err()
                        );
                        self.remove_by_name(&name);
                    }
                    else {
                        assert!(rmdir_result.is_err());
                    }
                }
                Operation::Rename(old_name, new_name) => {
                    if print {
                        error!("Rename: parent = {:?}, old_name = {:?}, target = {:?}, new_name = {:?}", self.name, old_name, self.name, new_name);
                    }
                    let rename_result = self.inode.rename(&old_name, &self.inode, &new_name);

                    if old_name.eq(&new_name) {
                        assert!(rename_result.is_ok());
                        return;
                    }

                    let mut valid_rename: bool = false;
                    let mut exist: bool = false;
                    if let Option::Some(old_sub) = self.sub_dirs.get(&old_name) {
                        let exist_new_sub = self.sub_dirs.get(&new_name);
                        match old_sub {
                            DentryInMemory::File(old_file) => {
                                if let Option::Some(exist_new_sub_) = exist_new_sub
                                    && let DentryInMemory::File(exist_new_file) = exist_new_sub_ {
                                    valid_rename = true;
                                    exist = true;
                                }
                                else if exist_new_sub.is_none() {
                                    valid_rename = true;
                                }
                            }
                            DentryInMemory::Dir(old_dir) => {
                                if let Option::Some(exist_new_sub_) = exist_new_sub
                                    && let DentryInMemory::Dir(exist_new_dir) = exist_new_sub_
                                    && exist_new_dir.sub_dirs.is_empty() {
                                    valid_rename = true;
                                    exist = true;
                                }
                                else if exist_new_sub.is_none() {
                                    valid_rename = true;
                                }
                            }
                        }
                    }

                    if valid_rename {
                        assert!(
                            rename_result.is_ok(),
                            "Fail to rename {:?}/{:?} to {:?}/{:?}: {:?}",
                            self.name,
                            old_name,
                            self.name,
                            new_name,
                            rename_result.unwrap_err()
                        );
                        let lookup_new_inode_result = self.inode.lookup(&new_name);
                        assert!(lookup_new_inode_result.is_ok());
                        let mut old = self.sub_dirs.remove(&old_name).unwrap();
                        self.remove_by_name(&old_name);
                        match old {
                            DentryInMemory::Dir(ref mut dir) => {
                                dir.inode = lookup_new_inode_result.unwrap();
                                dir.name = new_name.clone();
                                dir.depth = self.depth + 1;
                            }
                            DentryInMemory::File(ref mut file) => {
                                file.inode = lookup_new_inode_result.unwrap();
                                file.name = new_name.clone();
                            }
                        }
                        if exist {
                            self.sub_dirs.remove(&new_name);
                            self.remove_by_name(&new_name);
                        }
                        self.sub_dirs.insert(new_name.to_string(), old);
                        self.sub_names.push(new_name.to_string());
                    } else {
                        assert!(rename_result.is_err());
                    }
                }
                /*
                Operation::Rename(old_name, target, new_name) => {
                    error!("Rename: parent = {:?}, old_name = {:?}, target = {:?}, new_name = {:?}", self.name, old_name, target.name, new_name);
                    let rename_result = self.inode.rename(&old_name, &target.inode, &new_name);

                    let mut valid_rename: bool = false;
                    if let Option::Some(old_sub) = self.sub_dirs.get(&old_name) {
                        let exist_new_sub = target.sub_dirs.get(&new_name);
                        match old_sub {
                            DentryInMemory::File(old_file) => {
                                if let Option::Some(exist_new_sub_) = exist_new_sub
                                    && let DentryInMemory::File(exist_new_file) = exist_new_sub_ {
                                    valid_rename = true;
                                    target.remove_by_name(&new_name);
                                }
                                else if exist_new_sub.is_none() {
                                    valid_rename = true;
                                }
                            }
                            DentryInMemory::Dir(old_dir) => {
                                if let Option::Some(exist_new_sub_) = exist_new_sub
                                    && let DentryInMemory::Dir(exist_new_dir) = exist_new_sub_
                                    && exist_new_dir.sub_dirs.is_empty() {
                                    valid_rename = true;
                                    target.remove_by_name(&new_name);
                                }
                                else if exist_new_sub.is_none() {
                                    valid_rename = true;
                                }
                            }
                        }
                    }

                    if valid_rename {
                        assert!(rename_result.is_ok(),
                            "Fail to rename {:?} to {:?}: {:?}",
                            old_name, new_name, rename_result.unwrap_err()
                        );
                        let lookup_new_inode_result = target.inode.lookup(&new_name);
                        assert!(lookup_new_inode_result.is_ok());
                        let mut old = self.sub_dirs.remove(&old_name).unwrap();
                        match old {
                            DentryInMemory::Dir(ref mut dir) => {
                                dir.inode = lookup_new_inode_result.unwrap();
                                dir.depth = target.depth + 1;
                                dir.sub_names.push(new_name.clone());
                            }
                            DentryInMemory::File(ref mut file) => file.inode = lookup_new_inode_result.unwrap()
                        }
                        target.sub_dirs.insert(new_name, old);
                    }
                    else {
                        assert!(rename_result.is_err());
                    }
                }
                */
                _ => {
                    return;
                }
            }
        }
    }

    impl FileInMemory {
        pub fn execute_and_test(&mut self, op: Operation, print: bool) {
            match op {
                Operation::Read(offset, len) => {
                    if print {
                        error!(
                            "Read: name = {:?}, offset = {:?}, len = {:?}",
                            self.name, offset, len
                        );
                    }
                    let mut buf: Vec<u8> = Vec::new();
                    buf.resize(len, 0);
                    let read_result = self.inode.read_at(offset, &mut buf);
                    assert!(
                        read_result.is_ok(),
                        "Fail to read file in range [{:?}, {:?}): {:?}",
                        offset,
                        offset + len,
                        read_result.unwrap_err()
                    );
                    let (start, end) = (
                        offset.min(self.valid_len),
                        (offset + len).min(self.valid_len),
                    );
                    assert!(
                        buf[..(end - start)].eq(&self.contents[start..end]),
                        "Read file contents mismatch"
                    )
                }
                Operation::Write(offset, len) => {
                    if print {
                        error!(
                            "Write: name = {:?}, offset = {:?}, len = {:?}",
                            self.name, offset, len
                        );
                    }
                    let mut buf: Vec<u8> = Vec::new();
                    buf.resize(len, 0);
                    let _ = Random::getrandom(&mut buf);
                    let write_result = self.inode.write_at(offset, &buf);
                    assert!(
                        write_result.is_ok(),
                        "Fail to write file in range [{:?}, {:?}): {:?}",
                        offset,
                        offset + len,
                        write_result.unwrap_err()
                    );
                    if offset + len > self.contents.len() {
                        self.contents.resize(offset + len, 0);
                    }
                    self.valid_len = self.valid_len.max(offset + len);
                    for i in 0..len {
                        self.contents[offset + i] = buf[i];
                    }
                }
                Operation::Resize(new_size) => {
                    if print {
                        error!("Resize: name = {:?}, new_size = {:?}", self.name, new_size);
                    }
                    self.inode.resize(new_size);
                    self.contents.resize(new_size, 0);
                    self.valid_len = self.valid_len.min(new_size);
                }
                _ => {
                    return;
                }
            }
        }
    }

    impl DentryInMemory {
        pub fn execute_and_test(&mut self, op: Operation, print: bool) {
            match self {
                DentryInMemory::Dir(dir) => {
                    dir.execute_and_test(op, print);
                }
                DentryInMemory::File(file) => {
                    file.execute_and_test(op, print);
                }
            }
        }

        pub fn sub_cnt(&self) -> usize {
            match self {
                DentryInMemory::Dir(dir) => dir.sub_names.len(),
                DentryInMemory::File(file) => 0,
            }
        }
    }

    fn new_fs_in_memory(root: Arc<dyn Inode>) -> DentryInMemory {
        DentryInMemory::Dir(DirInMemory {
            depth: 0,
            name: (&"root").to_string(),
            inode: root,
            sub_names: Vec::new(),
            sub_dirs: HashMap::new(),
        })
    }

    // generate a random number in range [0, max]
    fn get_random_in(max: usize) -> usize {
        const MAX_RANDOM: usize = 0xFF;
        let mut random: [u8; 1] = [0];
        let _ = Random::getrandom(&mut random);
        let rand = random[0] as usize;
        return (rand * (max + 1) / MAX_RANDOM).min(max);
    }

    fn random_select_from_dir_tree(root: &mut DentryInMemory) -> &mut DentryInMemory {
        let sub_cnt = root.sub_cnt();
        if sub_cnt == 0 {
            return root;
        } else {
            let stop_get_deeper = get_random_in(1) > 0;
            if stop_get_deeper {
                return root;
            } else {
                if let DentryInMemory::Dir(dir) = root {
                    let sub_idx = get_random_in(sub_cnt - 1);
                    let sub = dir.sub_dirs.get_mut(&dir.sub_names[sub_idx]);
                    let sub_dir = sub.unwrap();
                    return random_select_from_dir_tree(sub_dir);
                } else {
                    assert!(false, "Reached an unexpected point");
                    todo!()
                }
            }
        }
    }

    fn generate_random_offset_len(max_size: usize) -> (usize, usize) {
        let offset = get_random_in(max_size - 1);
        let len = get_random_in(max_size - offset);
        (offset, len)
    }

    fn generate_random_operation(
        root: &mut DentryInMemory,
        idx: u32,
    ) -> (&mut DentryInMemory, Operation) {
        const CREATE_FILE_ID: usize = 0;
        const CREATE_DIR_ID: usize = 1;
        const UNLINK_ID: usize = 2;
        const RMDIR_ID: usize = 3;
        const LOOKUP_ID: usize = 4;
        const READDIR_ID: usize = 5;
        const RENAME_ID: usize = 6;
        const DIR_OP_NUM: usize = 7;
        const READ_ID: usize = 0;
        const WRITE_ID: usize = 1;
        const RESIZE_ID: usize = 2;
        const FILE_OP_NUM: usize = 3;
        const MAX_PAGE_PER_FILE: usize = 10;
        let dentry = random_select_from_dir_tree(root);
        match dentry {
            DentryInMemory::Dir(dir) => {
                let op_id = get_random_in(DIR_OP_NUM - 1);
                if op_id == CREATE_FILE_ID {
                    return (dentry, Operation::Create(idx.to_string(), InodeType::File));
                } else if op_id == CREATE_DIR_ID {
                    return (dentry, Operation::Create(idx.to_string(), InodeType::Dir));
                } else if op_id == UNLINK_ID && dir.sub_names.len() > 0 {
                    let rand_idx = get_random_in(dir.sub_names.len() - 1);
                    let name = dir.sub_names[rand_idx].clone();
                    return (dentry, Operation::Unlink(name));
                } else if op_id == RMDIR_ID && dir.sub_names.len() > 0 {
                    let rand_idx = get_random_in(dir.sub_names.len() - 1);
                    let name = dir.sub_names[rand_idx].clone();
                    return (dentry, Operation::Rmdir(name));
                } else if op_id == LOOKUP_ID && dir.sub_names.len() > 0 {
                    let rand_idx = get_random_in(dir.sub_names.len() - 1);
                    let name = dir.sub_names[rand_idx].clone();
                    return (dentry, Operation::Lookup(name));
                } else if op_id == READDIR_ID {
                    return (dentry, Operation::Readdir());
                } else if op_id == RENAME_ID && dir.sub_names.len() > 0 {
                    let rand_old_idx = get_random_in(dir.sub_names.len() - 1);
                    let old_name = dir.sub_names[rand_old_idx].clone();
                    let rename_to_an_exist = get_random_in(1) > 0;
                    if rename_to_an_exist {
                        let rand_new_idx = get_random_in(dir.sub_names.len() - 1);
                        let new_name = dir.sub_names[rand_new_idx].clone();
                        return (dentry, Operation::Rename(old_name, new_name));
                    } else {
                        return (dentry, Operation::Rename(old_name, idx.to_string()));
                    }
                    /*
                    let rand_old_idx = get_random_in(dir.sub_names.len() - 1);
                    let old_name = dir.sub_names[rand_old_idx].clone();
                    let target = random_select_from_dir_tree(root);
                    match target {
                        DentryInMemory::Dir(tar_dir) => {
                            if tar_dir.sub_names.len() > 0 {
                                let rand_new_idx = get_random_in(tar_dir.sub_names.len() - 1);
                                let new_name = dir.sub_names[rand_new_idx].clone();
                                return (dentry, Operation::Rename(old_name, tar_dir, new_name));
                            }
                            else {
                                return (dentry, Operation::Rename(old_name, tar_dir, idx.to_string()));
                            }
                        }
                        DentryInMemory::File(_) => {
                            if let DentryInMemory::Dir(root_dir) = root {
                                return (dentry, Operation::Rename(old_name, root_dir, idx.to_string()));
                            }
                            else {
                                todo!()
                            }
                        }
                    }
                    */
                } else {
                    return (dentry, Operation::Create(idx.to_string(), InodeType::File));
                }
            }
            DentryInMemory::File(file) => {
                let op_id = get_random_in(FILE_OP_NUM - 1);
                if op_id == READ_ID {
                    let (offset, len) = generate_random_offset_len(MAX_PAGE_PER_FILE * PAGE_SIZE);
                    return (dentry, Operation::Read(offset, len));
                } else if op_id == WRITE_ID {
                    let (offset, len) = generate_random_offset_len(MAX_PAGE_PER_FILE * PAGE_SIZE);
                    return (dentry, Operation::Write(offset, len));
                } else if op_id == RESIZE_ID {
                    let pg_num = get_random_in(MAX_PAGE_PER_FILE);
                    let new_size = (pg_num * PAGE_SIZE).max(file.contents.len());
                    return (dentry, Operation::Resize(new_size));
                } else {
                    let valid_len = file.valid_len;
                    return (dentry, Operation::Read(0, valid_len));
                }
            }
        }
    }

    use super::load_exfat;
    fn create_file(parent: Arc<dyn Inode>, filename: &str) -> Arc<dyn Inode> {
        let create_result = parent.create(
            filename,
            crate::fs::utils::InodeType::File,
            InodeMode::all(),
        );

        assert!(
            create_result.is_ok(),
            "Fs failed to create: {:?}",
            create_result.unwrap_err()
        );

        create_result.unwrap()
    }

    fn create_folder(parent: Arc<dyn Inode>, foldername: &str) -> Arc<dyn Inode> {
        let create_result = parent.create(
            foldername,
            crate::fs::utils::InodeType::Dir,
            InodeMode::all(),
        );

        assert!(
            create_result.is_ok(),
            "Fs failed to create: {:?}",
            create_result.unwrap_err()
        );

        create_result.unwrap()
    }

    #[ktest]
    fn test_new_exfat() {
        load_exfat();
    }

    #[ktest]
    fn test_create() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;

        // test basic create
        let file_name = "a.txt";
        create_file(root.clone(), file_name);
        let dir_name = "b";
        create_folder(root.clone(), dir_name);

        // test create with an exist name
        let create_file_with_an_exist_name = root.create(
            dir_name,
            crate::fs::utils::InodeType::File,
            InodeMode::all(),
        );
        let create_dir_with_an_exist_name = root.create(
            file_name,
            crate::fs::utils::InodeType::Dir,
            InodeMode::all(),
        );
        assert!(
            create_dir_with_an_exist_name.is_err() && create_file_with_an_exist_name.is_err(),
            "Fs deal with create an exist name incorrectly"
        );

        // test create with a long name
        let long_file_name = "x".repeat(MAX_NAME_LENGTH);
        let create_long_name_file = root.create(
            &long_file_name,
            crate::fs::utils::InodeType::File,
            InodeMode::all(),
        );
        assert!(
            create_long_name_file.is_ok(),
            "Fail to create a long name file"
        );

        let long_dir_name = "y".repeat(MAX_NAME_LENGTH);
        let create_long_name_dir = root.create(
            &long_dir_name,
            crate::fs::utils::InodeType::Dir,
            InodeMode::all(),
        );
        assert!(
            create_long_name_dir.is_ok(),
            "Fail to create a long name directory"
        );
    }

    #[ktest]
    fn test_create_and_list_file() {
        let mut file_names: Vec<String> = (0..100).map(|x| x.to_string().repeat(50)).collect();
        file_names.sort();

        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;

        for (file_id, file_name) in file_names.iter().enumerate() {
            create_file(root.clone(), file_name);

            let mut sub_inodes: Vec<String> = Vec::new();

            let read_result = root.readdir_at(0, &mut sub_inodes);
            assert!(
                read_result.is_ok(),
                "Fs failed to readdir: {:?}",
                read_result.unwrap_err()
            );

            assert!(read_result.unwrap() == file_id + 1);
            assert!(sub_inodes.len() == file_id + 1);

            sub_inodes.sort();

            for i in 0..sub_inodes.len() {
                assert!(
                    sub_inodes[i].cmp(&file_names[i]).is_eq(),
                    "Readdir Result:{:?} Filenames:{:?}",
                    sub_inodes[i],
                    file_names[i]
                )
            }

            info!("Successfully creating and reading {} files", file_id + 1);
        }

        //Test skiped readdir.
        let mut sub_inodes: Vec<String> = Vec::new();
        let _ = root.readdir_at(file_names.len() / 3, &mut sub_inodes);

        assert!(sub_inodes.len() == file_names.len() - file_names.len() / 3);
    }

    #[ktest]
    fn test_unlink_single() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let file_name = "a.txt";
        let a_inode = create_file(root.clone(), file_name);
        let _ = a_inode.write_at(8192, &[0, 1, 2, 3, 4]);

        let unlink_result = root.unlink(file_name);
        assert!(
            unlink_result.is_ok(),
            "Fs failed to unlink: {:?}",
            unlink_result.unwrap_err()
        );

        let mut sub_dirs: Vec<String> = Vec::new();
        let _ = root.readdir_at(0, &mut sub_dirs);

        assert!(sub_dirs.is_empty());

        // followings are some invalid unlink call. These should return with an error.
        let unlink_fail_result1 = root.unlink(".");
        assert!(
            unlink_fail_result1.is_err(),
            "Fs deal with unlink(.) incorrectly"
        );

        let unlink_fail_result2 = root.unlink("..");
        assert!(
            unlink_fail_result2.is_err(),
            "Fs deal with unlink(..) incorrectly"
        );

        let folder_name = "sub";
        create_folder(root.clone(), folder_name);
        let unlink_dir = root.unlink(folder_name);
        assert!(
            unlink_dir.is_err(),
            "Fs deal with unlink a folder incorrectly"
        );

        // test unlink a long name file
        let long_file_name = "x".repeat(MAX_NAME_LENGTH);
        create_file(root.clone(), &long_file_name);
        let unlink_long_name_file = root.unlink(&long_file_name);
        assert!(
            unlink_long_name_file.is_ok(),
            "Fail to unlink a long name file"
        );
    }

    #[ktest]
    fn test_unlink_multiple() {
        let file_num: u32 = 30; // This shouldn't be too large, better not allocate new clusters for root dir
        let mut file_names: Vec<String> = (0..file_num).map(|x| x.to_string()).collect();
        file_names.sort();

        let fs = load_exfat();
        let cluster_size = fs.cluster_size();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let mut free_clusters_before_create: Vec<u32> = Vec::new();
        for (file_id, file_name) in file_names.iter().enumerate() {
            free_clusters_before_create.push(fs.free_clusters());
            let inode = create_file(root.clone(), file_name);

            if fs.free_clusters() > file_id as u32 {
                let _ = inode.write_at(file_id * cluster_size, &[0, 1, 2, 3, 4]);
            }
        }

        let mut reverse_names = file_names.clone();
        reverse_names.reverse();
        for (file_id, file_name) in reverse_names.iter().enumerate() {
            let id = file_num as usize - 1 - file_id;
            let unlink_result = root.unlink(file_name);
            assert!(
                unlink_result.is_ok() && fs.free_clusters() == free_clusters_before_create[id],
                "Fail to unlink file {:?}",
                id
            );

            let mut sub_inodes: Vec<String> = Vec::new();

            let read_result = root.readdir_at(0, &mut sub_inodes);
            assert!(
                read_result.is_ok(),
                "Fail to readdir after unlink {:?}: {:?}",
                id,
                read_result.unwrap_err()
            );

            assert!(read_result.unwrap() == id);
            assert!(sub_inodes.len() == id);

            sub_inodes.sort();

            for i in 0..sub_inodes.len() {
                assert!(sub_inodes[i].cmp(&file_names[i]).is_eq())
            }
        }
    }

    #[ktest]
    fn test_rmdir() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let folder_name = "sub";
        create_folder(root.clone(), folder_name);
        let rmdir_result = root.rmdir(folder_name);
        assert!(
            rmdir_result.is_ok(),
            "Fail to rmdir: {:?}",
            rmdir_result.unwrap_err()
        );

        let mut sub_dirs: Vec<String> = Vec::new();
        let _ = root.readdir_at(0, &mut sub_dirs);
        assert!(sub_dirs.is_empty());

        // Followings are some invalid unlink call. These should return with an error.
        let rmdir_fail_result1 = root.rmdir(".");
        assert!(
            rmdir_fail_result1.is_err(),
            "Fs deal with rmdir(.) incorrectly"
        );

        let rmdir_fail_result2 = root.rmdir("..");
        assert!(
            rmdir_fail_result2.is_err(),
            "Fs deal with rmdir(..) incorrectly"
        );

        let file_name = "a.txt";
        create_file(root.clone(), file_name);
        let rmdir_to_a_file = root.rmdir(file_name);
        assert!(
            rmdir_to_a_file.is_err(),
            "Fs deal with rmdir to a file incorrectly"
        );

        let parent_name = "parent";
        let child_name = "child.txt";
        let parent_inode = create_folder(root.clone(), parent_name);
        create_file(parent_inode.clone(), child_name);
        let rmdir_no_empty_dir = root.rmdir(parent_name);
        assert!(
            rmdir_no_empty_dir.is_err(),
            "Fs deal with rmdir to a no empty directory incorrectly"
        );
        // however, after we remove child file, parent directory is removable.
        let _ = parent_inode.unlink(child_name);
        let rmdir_empty_dir = root.rmdir(parent_name);
        assert!(rmdir_empty_dir.is_ok(), "Fail to remove an empty directory");

        // test remove a long name directory
        let long_dir_name = "x".repeat(MAX_NAME_LENGTH);
        create_folder(root.clone(), &long_dir_name);
        let rmdir_long_name_dir = root.rmdir(&long_dir_name);
        assert!(
            rmdir_long_name_dir.is_ok(),
            "Fail to remove a long name directory"
        );
    }

    #[ktest]
    fn test_rename_file() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let file_name = "hi.txt";
        let a_inode = create_file(root.clone(), file_name);

        const BUF_SIZE: usize = 7 * PAGE_SIZE + 11;
        let mut buf = vec![0u8; BUF_SIZE];
        for (i, num) in buf.iter_mut().enumerate() {
            //Use a prime number to make each sector different.
            *num = (i % 107) as u8;
        }
        let _ = a_inode.write_at(0, &buf);

        let new_name = "hello.txt";
        let rename_result = root.rename(file_name, &root.clone(), new_name);
        assert!(
            rename_result.is_ok(),
            "Failed to rename: {:?}",
            rename_result.unwrap_err()
        );

        // test list after rename
        let mut sub_dirs: Vec<String> = Vec::new();
        let _ = root.readdir_at(0, &mut sub_dirs);
        assert!(sub_dirs.len() == 1 && sub_dirs[0].eq(new_name));

        // test read after rename
        let a_inode_new = root.lookup(new_name).unwrap();
        let mut read = vec![0u8; BUF_SIZE];
        let read_after_rename = a_inode_new.read_at(0, &mut read);
        assert!(
            read_after_rename.is_ok() && read_after_rename.clone().unwrap() == BUF_SIZE,
            "Fail to read after rename: {:?}",
            read_after_rename.unwrap_err()
        );
        assert!(buf.eq(&read), "File mismatch after rename");

        // test write after rename
        const NEW_BUF_SIZE: usize = 9 * PAGE_SIZE + 23;
        let new_buf = vec![7u8; NEW_BUF_SIZE];
        let new_write_after_rename = a_inode_new.write_at(0, &new_buf);
        assert!(
            new_write_after_rename.is_ok()
                && new_write_after_rename.clone().unwrap() == NEW_BUF_SIZE,
            "Fail to write file after rename: {:?}",
            new_write_after_rename.unwrap_err()
        );

        let mut new_read = vec![0u8; NEW_BUF_SIZE];
        let _ = a_inode_new.read_at(0, &mut new_read);
        assert!(
            new_buf.eq(&new_read),
            "New read and new write mismatch after rename"
        );

        // test rename between different directories
        let sub_folder_name = "test";
        let sub_folder = create_folder(root.clone(), sub_folder_name);
        let sub_file_name = "a.txt";
        create_file(sub_folder.clone(), sub_file_name);
        let rename_result = sub_folder.rename(sub_file_name, &root.clone(), sub_file_name);
        assert!(
            rename_result.is_ok(),
            "Fs failed to rename file between different directories: {:?}",
            rename_result.unwrap_err()
        );

        sub_dirs.clear();
        let _ = root.readdir_at(0, &mut sub_dirs);
        sub_dirs.sort();

        assert!(
            sub_dirs.len() == 3
                && sub_dirs[0].eq(sub_file_name)
                && sub_dirs[1].eq(new_name)
                && sub_dirs[2].eq(sub_folder_name)
        );

        // test rename file when the new_name is exist
        let rename_file_to_itself = root.rename(new_name, &root.clone(), new_name);
        assert!(rename_file_to_itself.is_ok(), "Fail to rename to itself");

        let rename_file_to_an_exist_folder = root.rename(new_name, &root.clone(), sub_folder_name);
        assert!(
            rename_file_to_an_exist_folder.is_err(),
            "Fs deal with rename a file to an exist directory incorrectly"
        );

        let rename_file_to_an_exist_file = root.rename(new_name, &root.clone(), sub_file_name);
        assert!(
            rename_file_to_an_exist_file.is_ok(),
            "Fail to rename a file to another exist file",
        );

        sub_dirs.clear();
        let _ = root.readdir_at(0, &mut sub_dirs);
        sub_dirs.sort();

        assert!(
            sub_dirs.len() == 2 && sub_dirs[0].eq(sub_file_name) && sub_dirs[1].eq(sub_folder_name)
        );
    }

    #[ktest]
    fn test_rename_dir() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let old_folder_name = "old_folder";
        let old_folder = create_folder(root.clone(), old_folder_name);
        let child_file_name = "a.txt";
        create_file(old_folder.clone(), child_file_name);

        // Test rename a folder, the sub-directories should remain.
        let new_folder_name = "new_folder";
        let rename_result = root.rename(old_folder_name, &root.clone(), new_folder_name);
        assert!(
            rename_result.is_ok(),
            "Fs failed to rename a folder: {:?}",
            rename_result.unwrap_err()
        );

        let mut sub_dirs: Vec<String> = Vec::new();
        let _ = root.readdir_at(0, &mut sub_dirs);
        assert!(sub_dirs.len() == 1 && sub_dirs[0].eq(new_folder_name));

        let new_folder = root.lookup(new_folder_name).unwrap();

        sub_dirs.clear();
        let _ = new_folder.readdir_at(0, &mut sub_dirs);
        assert!(sub_dirs.len() == 1 && sub_dirs[0].eq(child_file_name));

        // Test rename directory when the new_name is exist.
        let exist_folder_name = "exist_folder";
        let exist_folder = create_folder(root.clone(), exist_folder_name);
        create_file(exist_folder.clone(), child_file_name);

        let exist_file_name = "exist_file.txt";
        create_file(root.clone(), exist_file_name);

        let rename_dir_to_an_exist_file =
            root.rename(new_folder_name, &root.clone(), exist_file_name);
        assert!(rename_dir_to_an_exist_file.is_err());

        let rename_dir_to_an_exist_no_empty_folder =
            root.rename(new_folder_name, &root.clone(), exist_folder_name);
        assert!(rename_dir_to_an_exist_no_empty_folder.is_err());

        let _ = exist_folder.unlink(child_file_name);
        let rename_dir_to_an_exist_empty_folder =
            root.rename(new_folder_name, &root.clone(), exist_folder_name);
        assert!(rename_dir_to_an_exist_empty_folder.is_ok());
    }

    #[ktest]
    fn test_write_and_read_file_direct() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let file = create_file(root.clone(), "test");

        const BUF_SIZE: usize = PAGE_SIZE * 7 + 3 * SECTOR_SIZE;

        let mut buf = vec![0u8; BUF_SIZE];
        for (i, num) in buf.iter_mut().enumerate() {
            //Use a prime number to make each sector different.
            *num = (i % 107) as u8;
        }

        let write_result = file.write_direct_at(0, &buf);
        assert!(
            write_result.is_ok(),
            "Fs failed to write direct: {:?}",
            write_result.unwrap_err()
        );

        let mut read = vec![0u8; BUF_SIZE];
        let read_result = file.read_direct_at(0, &mut read);
        assert!(
            read_result.is_ok(),
            "Fs failed to read direct: {:?}",
            read_result.unwrap_err()
        );

        assert!(buf.eq(&read), "File mismatch. Data read result:{:?}", read);
    }

    #[ktest]
    fn test_write_and_read_file() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let file = create_file(root.clone(), "test");

        const BUF_SIZE: usize = PAGE_SIZE * 11 + 2023;

        let mut buf = vec![0u8; BUF_SIZE];
        for (i, num) in buf.iter_mut().enumerate() {
            //Use a prime number to make each sector different.
            *num = (i % 107) as u8;
        }

        let write_result = file.write_at(0, &buf);
        assert!(
            write_result.is_ok(),
            "Fs failed to write: {:?}",
            write_result.unwrap_err()
        );

        let mut read = vec![0u8; BUF_SIZE];
        let read_result = file.read_at(0, &mut read);
        assert!(
            read_result.is_ok(),
            "Fs failed to read: {:?}",
            read_result.unwrap_err()
        );

        assert!(buf.eq(&read), "File mismatch. Data read result:{:?}", read);
    }

    #[ktest]
    fn test_interleaved_write() {
        let fs = load_exfat();
        let root = fs.root_inode() as Arc<dyn Inode>;
        let a = create_file(root.clone(), "a");
        let b = create_file(root.clone(), "b");

        const BUF_SIZE: usize = PAGE_SIZE * 11 + 2023;

        let mut buf_a = vec![0u8; BUF_SIZE];
        for (i, num) in buf_a.iter_mut().enumerate() {
            //Use a prime number to make each sector different.
            *num = (i % 107) as u8;
        }

        let mut buf_b = vec![0u8; BUF_SIZE];
        for (i, num) in buf_b.iter_mut().enumerate() {
            //Use a prime number to make each sector different.
            *num = (i % 109) as u8;
        }

        let steps = 7;
        let write_len = (BUF_SIZE + steps - 1) / steps;
        for i in 0..steps {
            let start = i * write_len;
            let end = BUF_SIZE.min(start + write_len);
            a.write_at(start, &buf_a[start..end]).unwrap();
            b.write_at(start, &buf_b[start..end]).unwrap();
        }

        let mut read = vec![0u8; BUF_SIZE];
        a.read_at(0, &mut read).unwrap();
        assert!(
            buf_a.eq(&read),
            "File a mismatch. Data read result:{:?}",
            read
        );

        b.read_at(0, &mut read).unwrap();
        assert!(
            buf_b.eq(&read),
            "File b mismatch. Data read result:{:?}",
            read
        );
    }

    #[ktest]
    fn test_bitmap_modify_bit() {
        let fs = load_exfat();
        let bitmap_binding = fs.bitmap();
        let mut bitmap = bitmap_binding.lock();
        let total_bits_len = 1000;
        let initial_free_clusters = bitmap.free_clusters();

        let range_result =
            bitmap.find_next_free_cluster_range(EXFAT_RESERVED_CLUSTERS, total_bits_len);
        assert!(
            range_result.is_ok(),
            "Fail to get a free range with {:?} clusters",
            total_bits_len
        );

        let range_start_cluster = range_result.unwrap().start;
        let p = 107;
        for i in 0..total_bits_len {
            let relative_idx = (i * p) % total_bits_len;
            let idx = range_start_cluster + relative_idx;
            let res1 = bitmap.is_cluster_free(idx);
            assert!(
                res1.is_ok() && res1.unwrap(),
                "Cluster idx {:?} is set before set",
                relative_idx
            );

            let res2 = bitmap.set_bitmap_used(idx, true);
            assert!(
                res2.is_ok() && bitmap.free_clusters() == initial_free_clusters - 1,
                "Set cluster idx {:?} failed",
                relative_idx
            );

            let res3 = bitmap.is_cluster_free(idx);
            assert!(
                res3.is_ok() && !res3.unwrap(),
                "Cluster idx {:?} is unset after set",
                relative_idx
            );

            let res4 = bitmap.set_bitmap_unused(idx, true);
            assert!(
                res4.is_ok() && bitmap.free_clusters() == initial_free_clusters,
                "Clear cluster idx {:?} failed",
                relative_idx
            );

            let res5 = bitmap.is_cluster_free(idx);
            assert!(
                res5.is_ok() && res5.unwrap(),
                "Cluster idx {:?} is still set after clear",
                relative_idx
            );
        }
    }

    #[ktest]
    fn test_bitmap_modify_chunk() {
        let fs = load_exfat();
        let bitmap_binding = fs.bitmap();
        let mut bitmap = bitmap_binding.lock();
        let total_bits_len = 1000;
        let initial_free_clusters = bitmap.free_clusters();

        let range_result =
            bitmap.find_next_free_cluster_range(EXFAT_RESERVED_CLUSTERS, total_bits_len);
        assert!(
            range_result.is_ok(),
            "Fail to get a free range with {:?} clusters",
            total_bits_len
        );

        let range_start_idx = range_result.unwrap().start;
        let mut chunk_size = 1;
        let mut start_idx: u32 = range_start_idx;
        let mut end_idx = range_start_idx + 1;
        while end_idx <= range_start_idx + total_bits_len {
            let res1 = bitmap.set_bitmap_range_used(start_idx..end_idx, true);
            assert!(
                res1.is_ok() && bitmap.free_clusters() == initial_free_clusters - chunk_size,
                "Set cluster chunk [{:?}, {:?}) failed",
                start_idx,
                end_idx
            );

            for idx in start_idx..end_idx {
                let res = bitmap.is_cluster_free(idx);
                assert!(
                    res.is_ok() && !res.unwrap(),
                    "Cluster {:?} in chunk [{:?}, {:?}) is unset",
                    idx,
                    start_idx,
                    end_idx
                );
            }

            let res2 = bitmap.set_bitmap_range_unused(start_idx..end_idx, true);
            assert!(
                res2.is_ok() && bitmap.free_clusters() == initial_free_clusters,
                "Clear cluster chunk [{:?}, {:?}) failed",
                start_idx,
                end_idx
            );

            let res3 = bitmap.is_cluster_range_free(start_idx..end_idx);
            assert!(
                res3.is_ok() && res3.unwrap(),
                "Some bit in cluster chunk [{:?}, {:?}) is still set after clear",
                start_idx,
                end_idx
            );

            chunk_size += 1;
            start_idx = end_idx;
            end_idx = start_idx + chunk_size;
        }
    }

    #[ktest]
    fn test_bitmap_find() {
        let fs = load_exfat();
        let bitmap_binding = fs.bitmap();
        let mut bitmap = bitmap_binding.lock();
        let total_bits_len = 1000;

        let range_result =
            bitmap.find_next_free_cluster_range(EXFAT_RESERVED_CLUSTERS, total_bits_len);
        assert!(
            range_result.is_ok(),
            "Fail to get a free range with {:?} clusters",
            total_bits_len
        );

        let range_start_idx = range_result.unwrap().start;
        let mut chunk_size = 1;
        let mut start_idx;
        let mut end_idx = range_start_idx + 1;
        // 010010001000010000010000001...
        // chunk_size = k, relative_start_idx =(k-1)*(k+2)/2
        while end_idx <= range_start_idx + total_bits_len {
            let _ = bitmap.set_bitmap_used(end_idx, true);
            chunk_size += 1;
            start_idx = end_idx + 1;
            end_idx = start_idx + chunk_size;
        }

        for k in 1..chunk_size {
            let start_idx_k = bitmap.find_next_free_cluster_range(range_start_idx, k);
            assert!(
                start_idx_k.is_ok()
                    && start_idx_k.clone().unwrap().start
                        == (k - 1) * (k + 2) / 2 + range_start_idx
                    && start_idx_k.unwrap().end == (k * k + 3 * k - 2) / 2 + range_start_idx,
                "Fail to find chunk size {:?}",
                k
            );
        }

        for k in 1..chunk_size {
            let start_idx_k = bitmap.find_next_free_cluster_range_fast(range_start_idx, k);
            assert!(
                start_idx_k.is_ok()
                    && start_idx_k.clone().unwrap().start
                        == (k - 1) * (k + 2) / 2 + range_start_idx
                    && start_idx_k.unwrap().end == (k * k + 3 * k - 2) / 2 + range_start_idx,
                "Fail to find chunk size {:?} with fast",
                k
            );
        }
    }

    #[ktest]
    fn test_resize_single() {
        let fs = load_exfat();
        let root = fs.root_inode();
        let f = create_file(root.clone(), "xxx");
        let cluster_size = fs.cluster_size();
        let initial_free_clusters = fs.free_clusters();

        let max_clusters = 1000.min(initial_free_clusters);
        let mut alloc_clusters = 0;
        while alloc_clusters < max_clusters {
            alloc_clusters += 1;
            info!("alloc_clusters = {:?}", alloc_clusters);
            f.resize(alloc_clusters as usize * cluster_size);
            assert!(
                fs.free_clusters() == initial_free_clusters - alloc_clusters,
                "Fail to linearly expand file to {:?} clusters",
                alloc_clusters
            );
        }
        // here alloc_clusters == max_clusters

        while alloc_clusters > 0 {
            alloc_clusters -= 1;
            f.resize(alloc_clusters as usize * cluster_size);
            assert!(
                fs.free_clusters() == initial_free_clusters - alloc_clusters,
                "Fail to linearly shrink file to {:?} clusters",
                alloc_clusters
            );
        }

        alloc_clusters = 1;
        let mut old_alloc_clusters = 0;
        let mut step = 1;
        while alloc_clusters <= max_clusters {
            f.resize(alloc_clusters as usize * cluster_size);
            assert!(
                fs.free_clusters() == initial_free_clusters - alloc_clusters,
                "Fail to expand file from {:?} clusters to {:?} clusters",
                old_alloc_clusters,
                alloc_clusters
            );
            old_alloc_clusters = alloc_clusters;
            step += 1;
            alloc_clusters += step;
        }

        while alloc_clusters > 0 {
            alloc_clusters -= step;
            step -= 1;
            f.resize(alloc_clusters as usize * cluster_size);
            assert!(
                fs.free_clusters() == initial_free_clusters - alloc_clusters,
                "Fail to shrink file from {:?} clusters to {:?} clusters",
                old_alloc_clusters,
                alloc_clusters
            );
            old_alloc_clusters = alloc_clusters;
        }
        assert!(alloc_clusters == 0);

        // Try to allocate a file larger than remaining spaces. This will fail without changing the remaining space.
        f.resize(initial_free_clusters as usize * cluster_size + 1);
        assert!(
            fs.free_clusters() == initial_free_clusters,
            "Fail to deal with a memeory overflow allocation"
        );

        // Try to allocate a file of exactly the same size as the remaining spaces. This will succeed.
        f.resize(initial_free_clusters as usize * cluster_size);
        assert!(
            fs.free_clusters() == 0,
            "Fail to deal with a exact allocation"
        );

        // Free the file just allocated. This will also succeed.
        f.resize(0);
        assert!(
            fs.free_clusters() == initial_free_clusters,
            "Fail to free a large chunk"
        );
    }

    #[ktest]
    fn test_resize_multiple() {
        let fs = load_exfat();
        let cluster_size = fs.cluster_size();
        let root = fs.root_inode();
        let file_num: u32 = 45;
        let mut file_names: Vec<String> = (0..file_num).map(|x| x.to_string()).collect();
        file_names.sort();
        let mut file_inodes: Vec<Arc<dyn Inode>> = Vec::new();
        for (file_id, file_name) in file_names.iter().enumerate() {
            let inode = create_file(root.clone(), file_name);
            file_inodes.push(inode);
        }

        let initial_free_clusters = fs.free_clusters();
        let max_clusters = 10000.min(initial_free_clusters);
        let mut step = 1;
        let mut cur_clusters_per_file = 0;
        while file_num * (cur_clusters_per_file + step) <= max_clusters {
            for (file_id, inode) in file_inodes.iter().enumerate() {
                inode.resize((cur_clusters_per_file + step) as usize * cluster_size);
                assert!(
                    fs.free_clusters()
                        == initial_free_clusters
                            - cur_clusters_per_file * file_num
                            - (file_id as u32 + 1) * step,
                    "Fail to resize file {:?} from {:?} to {:?}",
                    file_id,
                    cur_clusters_per_file,
                    cur_clusters_per_file + step
                );
            }
            cur_clusters_per_file += step;
            step += 1;
        }
    }

    #[ktest]
    fn test_resize_write() {
        let fs = load_exfat();
        let root = fs.root_inode();
        let inode = root
            .create("xxx", InodeType::File, InodeMode::all())
            .unwrap();
        const MAX_PAGE_PER_FILE: usize = 20;

        let mut buf: Vec<u8> = Vec::new();
        let mut pg_num = 1;
        while pg_num <= MAX_PAGE_PER_FILE {
            let size = pg_num * PAGE_SIZE;
            inode.resize(size);

            buf.resize(size * PAGE_SIZE, 0);
            let _ = Random::getrandom(&mut buf);
            let write_result = inode.write_at(0, &buf);
            assert!(
                write_result.is_ok(),
                "Fail to write after resize expand from {:?}pgs to {:?}pgs: {:?}",
                pg_num - 1,
                pg_num,
                write_result.unwrap_err()
            );

            pg_num += 1;
        }

        pg_num = MAX_PAGE_PER_FILE;

        while pg_num > 0 {
            let size = (pg_num - 1) * PAGE_SIZE;
            inode.resize(size);

            buf.resize(size * PAGE_SIZE, 0);
            let _ = Random::getrandom(&mut buf);
            let write_result = inode.write_at(0, &buf);
            assert!(
                write_result.is_ok(),
                "Fail to write after resize shrink from {:?}pgs to {:?}pgs: {:?}",
                pg_num,
                pg_num - 1,
                write_result.unwrap_err()
            );

            pg_num -= 1;
        }
    }

    #[ktest]
    fn test_random_sequence() {
        let fs = load_exfat();
        let root = fs.root_inode();
        let mut fs_in_mem = new_fs_in_memory(root);

        let max_ops: u32 = 1000;
        for idx in 0..max_ops {
            let (file_or_dir, op) = generate_random_operation(&mut fs_in_mem, idx);
            file_or_dir.execute_and_test(op, false);
        }
    }
}
