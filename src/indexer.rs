use std::{ffi, mem, os::unix::ffi::OsStrExt, path};

use crate::{db, fanotify};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    DB(#[from] db::Error),
    #[error("OS error: {0}")]
    OS(#[from] errno::Errno),
    #[error("Nul error: {0}")]
    Nul(#[from] ffi::NulError),
    #[error("Provided root path doesn't match with DB")]
    BadRoot
}

unsafe extern "C" {
    fn name_to_handle_at(
        dirfd: libc::c_int,
        pathname: *const libc::c_char,
        handle: *mut fanotify::file_handle,
        mount_id: *mut libc::c_int,
        flags: libc::c_int
    ) -> libc::c_int;
}

fn get_fh(path: &path::Path) -> Result<(libc::c_int, Vec<u8>), Error> {
    let pathname = ffi::CString::new(path.as_os_str().as_bytes())?;
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
    if ret < 0 && errno::errno().0 != libc::EOVERFLOW {return Err(errno::errno().into())}
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
    if ret < 0 {return Err(errno::errno().into())};

    Ok((unsafe {mount_id.assume_init()}, buf))
}

pub fn index(conn: &mut rusqlite::Connection, path: &path::Path) -> Result<(), Error> {
    let mut tx = db::map_db_err(conn.transaction())?;

    db::ensure_root(&tx, &get_fh(path)?.1)?;

    for (i, entry) in walkdir::WalkDir::new(path).into_iter().enumerate() {
        let Ok(entry) = entry else {continue};
        if entry.path() == path {continue};
        let path = entry.path();
        let Some(parent) = path.parent() else {continue};
        let Some(filename) = path.file_name() else {continue};
        let Ok(filename) = filename.try_into() else {continue};

        let (_, fh) = match get_fh(path) {
            Ok(x) => x,
            Err(e) => {
                log::warn!(
                    "Failed to get file handle for {}: {}",
                    path.display(), e.to_string()
                );
                continue
            }
        };
        let (_, p_fh) = match get_fh(parent) {
            Ok(x) => x,
            Err(e) => {
                log::warn!(
                    "Failed to get file handle for {}: {}",
                    path.display(), e.to_string()
                );
                continue
            }
        };
        let parent_id = match db::get_single_id(&tx, &p_fh) {
            Ok(x) => x,
            Err(e) => {
                log::warn!("Directory of file handle for {} doesn't exist: {}",
                    parent.display(), e);
                continue
            }
        };

        match db::create(&tx, parent_id, &fh, filename) {
            Err(db::Error::DuplicateFile) => {
                // ignore
            }
            Err(e) => {
                log::warn!("{}", e.to_string());
                continue
            },
            _ => ()
        }

        if i % 1000 == 0 {
            log::info!("{} {}", i, path.display());
        }
        if i % 10000 == 0 {
            db::map_db_err(tx.commit())?;
            tx = db::map_db_err(conn.transaction())?;
        }
    }

    db::map_db_err(tx.commit())?;

    Ok(())
}
