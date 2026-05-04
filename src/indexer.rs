use std::{ffi, io, mem, os::unix::ffi::OsStrExt, path};

use anyhow::Context;

use crate::{db, fanotify};

unsafe extern "C" {
    fn name_to_handle_at(
        dirfd: libc::c_int,
        pathname: *const libc::c_char,
        handle: *mut fanotify::file_handle,
        mount_id: *mut libc::c_int,
        flags: libc::c_int
    ) -> libc::c_int;
}

fn get_fh(path: &path::Path) -> Result<(libc::c_int, Vec<u8>), io::Error> {
    let pathname = ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

    let mut fh = fanotify::file_handle {
        handle_bytes: 0,
        handle_type: 0,
        f_handle: []
    };

    let mut mount_id = mem::MaybeUninit::uninit();

    let ret = unsafe {name_to_handle_at(
        libc::AT_FDCWD,
        pathname.as_ptr(),
        &mut fh,
        mount_id.as_mut_ptr(),
        0
    )};

    let last_err = io::Error::last_os_error();

    if ret < 0 && last_err.raw_os_error() != Some(libc::EOVERFLOW) {
        return Err(last_err)
    }
    let fhsize = size_of::<fanotify::file_handle>() + fh.handle_bytes as usize;
    let mut buf = vec![0 as u8; fhsize];
    let fh_ptr = buf.as_mut_ptr() as *mut fanotify::file_handle;
    unsafe {(*fh_ptr).handle_bytes = fhsize as u32};

    let ret = unsafe {name_to_handle_at(
        libc::AT_FDCWD,
        pathname.as_ptr(),
        buf.as_mut_ptr() as *mut fanotify::file_handle,
        mount_id.as_mut_ptr(),
        0
    )};
    if ret < 0 {return Err(io::Error::last_os_error())};

    Ok((unsafe {mount_id.assume_init()}, buf))
}

fn index_path(tx: &mut rusqlite::Transaction, path: &path::Path) -> Result<(), anyhow::Error> {
    let Some(parent) = path.parent() else {
        return Err(anyhow::Error::msg(
            format!("Failed to get parent of path `{}`", path.display())
        ));
    };

    let Some(filename) = path.file_name() else {
        return Err(anyhow::Error::msg(
            format!("Failed to get filename of path `{}`", path.display())
        ));
    };

    let (_, fh) = get_fh(path)
        .with_context(|| format!("Failed to get file handle of `{}`", path.display()))?;

    let (_, p_fh) = get_fh(parent)
        .with_context(|| format!("Failed to get file handle of `{}`", parent.display()))?;

    let parent_id = db::get_single_id(&tx, &p_fh)
        .with_context(|| format!("Failed to get database ID of `{}`", parent.display()))?;

    match db::create(&tx, parent_id, &fh, filename) {
        Err(db::Error::DuplicateFile) => {
            // ignore
        }
        Err(e) => {
            return Err(e.into())
        },
        _ => ()
    }

    Ok(())
}

pub fn index(conn: &mut rusqlite::Connection, path: &path::Path) -> Result<(), anyhow::Error> {
    let mut tx = conn.transaction()?;

    db::ensure_root(&tx, &get_fh(path)?.1)?;

    for (i, entry) in walkdir::WalkDir::new(path).into_iter().enumerate() {
        let Ok(entry) = entry else {continue};
        if entry.path() == path {continue};

        let path = entry.path();

        if let Err(e) = index_path(&mut tx, path) {
            log::warn!("{}", e);
        }

        if i % 1000 == 0 {
            log::info!("{} {}", i, path.display());
        }
        if i % 10000 == 0 {
            db::map_db_err(tx.commit())?;
            tx = db::map_db_err(conn.transaction())?;
        }
    }

    tx.commit()?;

    Ok(())
}
