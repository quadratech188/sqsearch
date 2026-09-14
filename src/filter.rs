use std::{collections::HashSet, ffi::{OsStr, OsString}, fs, io, path};

use crate::{file_handle::{FileHan, FileHandleOps}, util};

#[derive(serde::Deserialize)]
struct Config {
    ignores: Vec<String>
}

#[derive(Debug)]
pub struct Filter {
    ignores: HashSet<OsString>,
    btrfs_subvol: Option<u64>
}

impl Filter {
    pub fn load(p: &path::Path) -> anyhow::Result<Self> {
        let config = fs::read(p)?;
        let config: Config = toml::from_slice(&config)?;

        Ok(Self {
            ignores: config.ignores.iter().map(|x| OsString::from(x)).collect(),
            btrfs_subvol: None
        })
    }
    pub fn add_btrfs_subvol(&mut self, subvol: &path::Path) -> io::Result<()> {
        self.btrfs_subvol = Some(
            get_root_objectid(&util::get_fh(subvol)?.1)
        );
        Ok(())
    }
    pub fn allow(&self, fh: &FileHan, p: Option<(&FileHan, &OsStr)>) -> bool {
        if let Some(root_objectid) = self.btrfs_subvol {
            if get_root_objectid(fh) != root_objectid {return false}
        }

        if let Some((_p_fh, name)) = p {
            if self.ignores.contains(name) {return false}
        }

        true
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
