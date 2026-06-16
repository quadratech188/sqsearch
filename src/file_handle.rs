use std::slice;

use crate::util;

#[derive(Clone, Debug)]
pub struct FileHandle {
    pub handle_type: libc::c_int,
    pub f_handle: Vec<libc::c_uchar>
}

impl FileHandle {
    pub fn from_kernel(buf: &[u8]) -> Option<FileHandle> {
        if buf.len() < size_of::<util::file_handle>() {
            return None
        }

        let ptr = buf.as_ptr() as *const util::file_handle;

        let handle_bytes = unsafe {(*ptr).handle_bytes} as usize;

        if buf.len() != size_of::<util::file_handle>() + handle_bytes {
            return None
        }

        let f_handle = unsafe {slice::from_raw_parts(
            (*ptr).f_handle.as_ptr(), handle_bytes
        )}.to_vec();

        Some(FileHandle {
            handle_type: unsafe {(*ptr).handle_type},
            f_handle
        })
    }
}

impl rusqlite::types::ToSql for FileHandle {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let mut buf = vec![0 as u8; 8 + self.f_handle.len()];

        // SQLite uses big-endian
        buf[..4].copy_from_slice(&(self.f_handle.len() as u32).to_be_bytes());
        buf[4..8].copy_from_slice(&(self.handle_type as i32).to_be_bytes());
        buf[8..].copy_from_slice(&self.f_handle);

        Ok(rusqlite::types::ToSqlOutput::from(buf))
    }
}

impl rusqlite::types::FromSql for FileHandle {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let buf = value.as_blob()?;
        if buf.len() < 8 {
            return Err(rusqlite::types::FromSqlError::InvalidBlobSize {
                expected_size: 8, blob_size: buf.len()
            })
        }

        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        let handle_type = i32::from_be_bytes(buf[..4].try_into().unwrap());

        if buf.len() != 8 + len {
            return Err(rusqlite::types::FromSqlError::InvalidBlobSize {
                expected_size: 8 + len, blob_size: buf.len()
            })
        }

        Ok(FileHandle {
            handle_type,
            f_handle: buf[8..].to_vec()
        })
    }
}
