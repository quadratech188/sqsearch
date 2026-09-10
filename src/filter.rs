use std::{io, path};

use crate::{file_handle::{FileHan, FileHandleOps}, util};

#[derive(Debug)]
pub struct Filter {
    btrfs_subvol: Option<u64>
}

impl Filter {
    pub fn new() -> Self {
        Self {
            btrfs_subvol: None
        }
    }
    pub fn add_btrfs_subvol(&mut self, subvol: &path::Path) -> io::Result<()> {
        self.btrfs_subvol = Some(
            get_root_objectid(&util::get_fh(subvol)?.1)
        );
        Ok(())
    }
    pub fn allow(&self, handle: &FileHan) -> bool {
        let Some(root_objectid) = self.btrfs_subvol else {return true};
        get_root_objectid(handle) == root_objectid
    }
}

// https://codebrowser.dev/linux/linux/fs/btrfs/export.h.html
#[allow(nonstandard_style)]

#[repr(packed)]
struct btrfs_fid_header {
    _objectid: u64,
    root_objectid: u64,
    _gen: u32,
}

#[warn(nonstandard_style)]

fn get_root_objectid(handle: &FileHan) -> u64 {
    let fid: btrfs_fid_header = util::read_as_type(handle.f_handle());
    fid.root_objectid
}
