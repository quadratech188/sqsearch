use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path, ptr};
use crate::file_handle::{FileHan, FileHandle};

pub fn read_as_type<T>(buf: &[u8]) -> T {
    if size_of::<T>() > buf.len() {
        panic!("Buffer ended early");
    }
    unsafe {ptr::read_unaligned(buf.as_ptr() as *const T)}
}


pub fn get_fh(path: &path::Path) -> Result<(libc::c_int, FileHandle), io::Error> {
    let pathname = CString::new(path.as_os_str().as_bytes()).unwrap();

    let mut mount_id = 0;
    let mut buf = vec![0; libc::MAX_HANDLE_SZ as usize];
    let fh_ptr = buf.as_mut_ptr() as *mut libc::file_handle;
    unsafe {(*fh_ptr).handle_bytes = libc::MAX_HANDLE_SZ as u32};


    let ret = unsafe {libc::name_to_handle_at(
        libc::AT_FDCWD,
        pathname.as_ptr(),
        fh_ptr,
        &mut mount_id,
        libc::AT_HANDLE_FID
    )};

    if ret == -1 {
        return Err(io::Error::last_os_error())
    }
    Ok((
        mount_id,
        FileHan::read_from_buf(&buf).expect("Invalid file handle").to_owned()
    ))
}
