use std::{ffi, io, mem, os::unix::ffi::OsStrExt, path};

use crate::file_handle::FileHandle;

#[repr(C)]
pub struct file_handle {
    pub handle_bytes: libc::c_uint,
    pub handle_type: libc::c_int,
    pub f_handle: [libc::c_uchar; 0]
}

unsafe extern "C" {
    fn name_to_handle_at(
        dirfd: libc::c_int,
        pathname: *const libc::c_char,
        handle: *mut file_handle,
        mount_id: *mut libc::c_int,
        flags: libc::c_int
    ) -> libc::c_int;
}

pub fn get_fh(path: &path::Path) -> Result<(libc::c_int, FileHandle), io::Error> {
    let pathname = ffi::CString::new(path.as_os_str().as_bytes()).unwrap();

    let mut fh = file_handle {
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
    let fhsize = size_of::<file_handle>() + fh.handle_bytes as usize;
    let mut buf = vec![0 as u8; fhsize];
    let fh_ptr = buf.as_mut_ptr() as *mut file_handle;
    unsafe {(*fh_ptr).handle_bytes = fhsize as u32};

    let ret = unsafe {name_to_handle_at(
        libc::AT_FDCWD,
        pathname.as_ptr(),
        buf.as_mut_ptr() as *mut file_handle,
        mount_id.as_mut_ptr(),
        0
    )};
    if ret < 0 {return Err(io::Error::last_os_error())};

    Ok((
        unsafe {mount_id.assume_init()},
        FileHandle::from_kernel(&buf).expect("Malformed file handle!")
    ))
}
