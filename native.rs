#[repr(C)]
pub struct file_handle {
    pub handle_bytes: libc::c_uint,
    pub handle_type: libc::c_int,
    pub f_handle: [libc::c_uchar; 0]
}
