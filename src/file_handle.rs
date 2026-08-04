use std::{borrow::Borrow, ops::Deref};


use crate::util;

#[derive(Debug)]
pub struct FileHandleInvalidError;

// FileHan is to FileHandle what str to String
#[repr(transparent)]
pub struct FileHan([u8]);

impl FileHan {
    pub fn read_from_buf(buf: &[u8]) -> Result<&Self, FileHandleInvalidError> {
        let header: util::file_handle = util::read_as_type(buf);
        let total_len = size_of::<util::file_handle>() + header.handle_bytes as usize;

        if total_len > buf.len() {
            return Err(FileHandleInvalidError)
        }

        Ok(unsafe {Self::unchecked(&buf[..total_len])})
    }

    pub unsafe fn unchecked(buf: &[u8]) -> &Self {
        unsafe {&*(buf as *const [u8] as *const FileHan)}
    }
}

#[derive(Clone, Debug)]
pub struct FileHandle(Vec<u8>);

impl FileHandle {
    pub fn new(handle: &FileHan) -> Self {
        Self(handle.buf().to_vec())
    }
}

impl Borrow<FileHan> for FileHandle {
    fn borrow(&self) -> &FileHan {
        unsafe {FileHan::unchecked(&self.0)}
    }
}
impl ToOwned for FileHan {
    type Owned = FileHandle;

    fn to_owned(&self) -> Self::Owned {
        FileHandle::new(self)
    }
}
impl Deref for FileHandle {
    type Target = FileHan;
    fn deref(&self) -> &Self::Target {
        unsafe {FileHan::unchecked(&self.0)}
    }
}

impl FileHandleOps for FileHan    {fn buf(&self) -> &[u8] {&self.0}}
impl FileHandleOps for FileHandle {fn buf(&self) -> &[u8] {&self.0}}

pub trait FileHandleOps {
    fn buf(&self) -> &[u8];

    fn f_handle(&self) -> &[u8] {
        &self.buf()[size_of::<util::file_handle>()..]
    }

    fn size(&self) -> usize {
        let header: util::file_handle = util::read_as_type(self.buf());
        size_of::<util::file_handle>() + header.handle_bytes as usize
    }
}

impl rusqlite::types::ToSql for FileHan {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.buf()))
    }
}

impl rusqlite::types::FromSql for FileHandle {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        FileHan::read_from_buf(value.as_blob()?)
            .map(|x| x.to_owned())
            .map_err(|_| rusqlite::types::FromSqlError::InvalidType)
    }
}
