use std::{ffi::{self, OsStr}, fs, io::{self, Read}, ptr};

use crate::{file_handle::FileHandle, util};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Read error: {0}")]
    Read(#[from] io::Error),
    #[error("Logic error")]
    Logic
}

#[derive(Debug)]
pub struct Event<'a> {
    buf: &'a [u8],
    mask: u64,
    fid: usize,
    dfid: Option<usize>,
    move_dfids: Option<(usize, usize)>
}

pub struct Create<'a> {
    buf: &'a [u8],
    dfid: usize
}

pub type Delete<'a> = Create<'a>;

pub struct Move<'a> {
    buf: &'a [u8],
    old_dfid: usize,
    new_dfid: usize
}

pub struct Reader {
    events: fs::File,
    buf: [u8; 4096]
}

fn read_as_type<T>(buf: &[u8]) -> Result<T, Error> {
    if size_of::<T>() > buf.len() {
        return Err(Error::Logic)
    }
    Ok(unsafe {ptr::read_unaligned(buf.as_ptr() as *const T)})
}

impl<'a> Event<'a> {
    fn from_slice(buf: &'a [u8]) -> Result<(Self, usize), Error> {
        let mut ptr = 0;

        let metadata = read_as_type::<libc::fanotify_event_metadata>(&buf[ptr..])?;
        let event_len = ptr + metadata.event_len as usize;
        let mask = metadata.mask;


        ptr += size_of::<libc::fanotify_event_metadata>();

        let mut fid = None;
        let mut dfid = None;
        let mut old_dfid = None;
        let mut new_dfid = None;

        while ptr < event_len {
            let header = read_as_type::<libc::fanotify_event_info_header>(&buf[ptr..])?;
            match header.info_type {
                libc::FAN_EVENT_INFO_TYPE_FID => fid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_DFID_NAME => dfid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => old_dfid = Some(ptr),
                libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => new_dfid = Some(ptr),
                _ => return Err(Error::Logic)
            }
            ptr += header.len as usize;
        }

        let Some(fid) = fid else {return Err(Error::Logic)};
        let rename_dfids = match (old_dfid, new_dfid) {
            (Some(x), Some(y)) => Some((x, y)),
            (None, None) => None,
            _ => return Err(Error::Logic)
        };

        Ok((Self {
            buf: &buf[..event_len],
            mask,
            fid,
            dfid,
            move_dfids: rename_dfids
        }, event_len))
    }

    pub fn as_create(&self) -> Result<Option<Create<'_>>, Error> {
        if self.mask & libc::FAN_CREATE == 0 {
            return Ok(None)
        }
        let Some(dfid) = self.dfid else {return Err(Error::Logic)};
        Ok(Some(Create {
            buf: self.buf,
            dfid
        }))
    }

    pub fn as_delete(&self) -> Result<Option<Delete<'_>>, Error> {
        if self.mask & libc::FAN_DELETE == 0 {
            return Ok(None)
        }
        let Some(dfid) = self.dfid else {return Err(Error::Logic)};
        Ok(Some(Delete {
            buf: self.buf,
            dfid
        }))
    }

    pub fn as_move(&self) -> Result<Option<Move<'_>>, Error> {
        if self.mask & libc::FAN_RENAME == 0 {
            return Ok(None)
        }
        let Some((old_dfid, new_dfid)) = self.move_dfids else {return Err(Error::Logic)};
        Ok(Some(Move {
            buf: self.buf,
            old_dfid,
            new_dfid
        }))
    }
}

fn get_fh<'a>(buf: &'a [u8]) -> Result<FileHandle, Error> {
    let begin = size_of::<libc::fanotify_event_info_fid>();
    let ptr = unsafe {(buf.as_ptr() as *const u8)
        .add(begin)};

    let handle_bytes = unsafe {ptr::read_unaligned(ptr as *const u32)} as usize;
    let len = size_of::<util::file_handle>() + handle_bytes;

    if buf.len() < len {return Err(Error::Logic)}
    Ok(FileHandle::from_kernel(&buf[begin..begin + len]).unwrap())
}

fn get_name<'a>(buf: &'a [u8]) -> Result<&'a OsStr, Error> {
    let ptr = unsafe {(buf.as_ptr() as *const u8)
        .add(size_of::<libc::fanotify_event_info_fid>())};

    let handle_bytes = unsafe {ptr::read_unaligned(ptr as *const u32)} as usize;

    // FIXME: Make sure CStr doesn't overflow

    let offset = size_of::<util::file_handle>() + handle_bytes as usize;
    let c_str = unsafe {ffi::CStr::from_ptr((ptr as *const i8).add(offset))};

    Ok(unsafe {OsStr::from_encoded_bytes_unchecked(c_str.to_bytes())})
}

impl<'a> Event<'a> {
    pub fn fh(&self) -> Result<FileHandle, Error> {get_fh(&self.buf[self.fid..])}
}

impl<'a> Create<'a> {
    pub fn p_fh(&self) -> Result<FileHandle, Error> {get_fh(&self.buf[self.dfid..])}
    pub fn name(&self) -> Result<&'a OsStr, Error> {get_name(&self.buf[self.dfid..])}
}

impl<'a> Move<'a> {
    pub fn old_p_fh(&self) -> Result<FileHandle, Error> {get_fh(&self.buf[self.old_dfid..])}
    pub fn old_name(&self) -> Result<&'a OsStr, Error> {get_name(&self.buf[self.old_dfid..])}
    pub fn new_p_fh(&self) -> Result<FileHandle, Error> {get_fh(&self.buf[self.new_dfid..])}
    pub fn new_name(&self) -> Result<&'a OsStr, Error> {get_name(&self.buf[self.new_dfid..])}
}

impl Reader {
    pub fn new(file: fs::File) -> Self {
        Self {
            events: file,
            buf: [0; 4096]
        }
    }
    pub fn map_events<F>(&mut self, f: &mut F) -> Result<(), Error>
    where F: FnMut(Event) -> Result<(), Error> {
        let len = self.events.read(&mut self.buf)?;
        let mut ptr = 0;

        while ptr < len {
            let (event, len) = Event::from_slice(&self.buf[ptr..])?;
            f(event)?;
            ptr += len;
        }

        Ok(())
    }
}
