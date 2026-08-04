use std::path;

use crate::{file_handle::{FileHan, FileHandleOps}, util};

#[derive(clap::Args, Debug, Clone)]
pub struct WatchPathArgs {
    /// Any path within the filesystem / subvolume you want to watch
    path: path::PathBuf,

    /// If watching a BTRFS subvolume, mount point of the root subvolume.
    #[arg(long)]
    btrfs_root: Option<path::PathBuf>
}

#[derive(Debug)]
pub enum Filter {
    None,
    Subvol {
        root_objectid: u64
    }
}


impl Filter {
    pub fn apply(&self, handle: &FileHan) -> bool {
        match self {
            Filter::None => true,
            Filter::Subvol { root_objectid } => {
                get_root_objectid(handle) == *root_objectid
            }
        }
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

pub fn prepare_fanotify(args: &WatchPathArgs)
-> Result<(path::PathBuf, Filter), anyhow::Error> {
    let Some(ref btrfs_root) = args.btrfs_root else {
        return Ok((args.path.clone(), Filter::None))
    };

    let (_, child_fh) = util::get_fh(&args.path)?;
    let (_, _root_fh) = util::get_fh(&btrfs_root)?;

    // TODO: Reimplement fsid checking

    /*
    if get_root_objectid(&child_fh) != get_root_objectid(&root_fh) {
        anyhow::bail!("Provided path and btrfs_root don't belong to the same BTRFS filesystem");
    }
    */

    Ok((btrfs_root.clone(), Filter::Subvol { root_objectid: get_root_objectid(&child_fh) }))
}
