use core::slice;
use std::{ffi::{self, OsString}, fs, io::{self, Read}, mem, ops, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, vec};

use libc;

use crate::{file_handle::FileHandle, util, watchpath};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Bad fanotify data")]
    BadData,
    #[error("From fanotify: {0}")]
    Internal(i32),
    #[error("While fanotify init: {0}")]
    Init(io::Error),
    #[error("While fanotify mark: {0}")]
    Mark(io::Error)
}

#[derive(Debug, Clone)]
pub enum Event {
    Create {
        p_fh: FileHandle,
        fh: FileHandle,
        name: OsString
    },
    Delete {
        p_fh: FileHandle,
        fh: FileHandle,
        name: OsString
    },
    Move {
        old_p_fh: FileHandle,
        new_p_fh: FileHandle,
        fh: FileHandle,
        old_name: OsString,
        new_name: OsString,
    }
}

fn get_handle(fid: &libc::fanotify_event_info_fid) -> FileHandle {
    let handle = fid.handle.as_ptr() as *const util::file_handle;
    let begin = handle as *const u8;
    let size = size_of::<util::file_handle>() + unsafe {(*handle).handle_bytes} as usize;

    FileHandle::from_kernel(unsafe {slice::from_raw_parts(begin, size)})
        .expect("Malformed file handle!")
}

fn get_name(fid: &libc::fanotify_event_info_fid) -> OsString {
    unsafe {
        let handle = fid.handle.as_ptr() as *const util::file_handle;
        let f_handle = (*handle).f_handle.as_ptr() as *const libc::c_uchar;
        let ptr = f_handle.add((*handle).handle_bytes as usize) as *const libc::c_char;
        let c_str = ffi::CStr::from_ptr(ptr);
        // TODO: Remove unnecessary copy
        OsString::from_encoded_bytes_unchecked(c_str.to_bytes().to_vec())
    }
}

fn read(buffer: &[u8], ptr: usize, filter: &watchpath::Filter, out: &mut Vec<Event>)
-> Result<usize, Error> {
    macro_rules! read_buffer {
        ($buffer: expr, $ptr: expr, $typ: ty) => {{
            let slice: &[u8; size_of::<$typ>()] = $buffer[$ptr..$ptr + size_of::<$typ>()]
                .try_into()
                .expect("Length should be fine");
            let result: &$typ = unsafe {
                mem::transmute(slice)
            };
            (result, $ptr + size_of::<$typ>())
        }};
    }

    let (metadata, n_ptr) = read_buffer!(&buffer, ptr, libc::fanotify_event_metadata);
    let next_event = ptr + metadata.event_len as usize;
    let mut ptr = n_ptr;

    let mut fid = None;
    let mut dfid = None;
    let mut new_dfid = None;
    let mut old_dfid = None;
    
    while ptr < next_event {
        let (header, _) = read_buffer!(buffer, ptr, libc::fanotify_event_info_header);
        match header.info_type {
            libc::FAN_EVENT_INFO_TYPE_DFID_NAME => dfid
                = Some(read_buffer!(buffer, ptr, libc::fanotify_event_info_fid).0),
            libc::FAN_EVENT_INFO_TYPE_FID => fid
                = Some(read_buffer!(buffer, ptr, libc::fanotify_event_info_fid).0),
            libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME => new_dfid
                = Some(read_buffer!(buffer, ptr, libc::fanotify_event_info_fid).0),
            libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME => old_dfid
                = Some(read_buffer!(buffer, ptr, libc::fanotify_event_info_fid).0),

            libc::FAN_EVENT_INFO_TYPE_ERROR => {
                return Err(Error::Internal(
                    read_buffer!(buffer, ptr, libc::fanotify_event_info_error).0.error
                ));
            }

            libc::FAN_EVENT_INFO_TYPE_DFID
            | libc::FAN_EVENT_INFO_TYPE_PIDFD
            | _ => return Err(Error::BadData)
        }
        ptr += header.len as usize;
    }

    let Some(fid) = fid else {return Err(Error::BadData)};
    let fh = get_handle(fid);

    if !filter.apply(&fh) {return Ok(next_event)}

    if metadata.mask & libc::FAN_CREATE != 0 {
        let Some(dfid) = dfid else {return Err(Error::BadData)};

        out.push(Event::Create {
            p_fh: get_handle(dfid),
            fh: fh.clone(),
            name: get_name(dfid)
        });
    }

    if metadata.mask & libc::FAN_DELETE != 0 {
        let Some(dfid) = dfid else {return Err(Error::BadData)};

        out.push(Event::Delete {
            p_fh: get_handle(dfid),
            fh: fh.clone(),
            name: get_name(dfid)
        });
    }

    if metadata.mask & libc::FAN_RENAME != 0 {
        let Some(old_dfid) = old_dfid else {return Err(Error::BadData)};
        let Some(new_dfid) = new_dfid else {return Err(Error::BadData)};

        out.push(Event::Move {
            old_p_fh: get_handle(old_dfid),
            new_p_fh: get_handle(new_dfid),
            fh: fh.clone(),
            old_name: get_name(old_dfid),
            new_name: get_name(new_dfid)
        });
    }

    Ok(next_event)
}

pub fn watch<F>(path: &path::Path, filter: &watchpath::Filter, callback: &mut F)
-> Result<(), Error>
where F: FnMut(&Vec<Event>) -> ops::ControlFlow<()> {

    let fd = unsafe {libc::fanotify_init(
        libc::FAN_CLASS_NOTIF
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_REPORT_FID
        | libc::FAN_REPORT_NAME
        | libc::FAN_REPORT_TARGET_FID
        | libc::FAN_REPORT_DIR_FID,
        libc::O_LARGEFILE as u32
    )};
    if fd < 0 {
        return Err(Error::Init(io::Error::last_os_error()))
    }

    let result = unsafe {libc::fanotify_mark(fd,
        libc::FAN_MARK_ADD
        | libc::FAN_MARK_FILESYSTEM,
        libc::FAN_ONDIR
        | libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_RENAME,
        libc::AT_FDCWD,
        ffi::CString::new(path.as_os_str().as_bytes())
            .unwrap()
            .as_ptr()
    )};
    if result < 0 {
        return Err(Error::Mark(io::Error::last_os_error()))
    }

    log::info!("Watching {}", path.display());

    let mut file = unsafe {fs::File::from_raw_fd(fd)};

    let mut buffer = [0; 4096];

    let mut events = vec![];

    loop {
        let read_len = file.read(&mut buffer).expect("Failed to read");
        let mut ptr = 0;

        while ptr < read_len {
            ptr = read(&buffer, ptr, filter, &mut events)?;
        }

        if events.len() == 0 {continue}

        let result = callback(&events);
        events.clear();

        match result {
            ops::ControlFlow::Continue(_) => continue,
            ops::ControlFlow::Break(_) => break
        }
    }

    Ok(())
}
