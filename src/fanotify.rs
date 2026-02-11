use std::{ffi, fs, io::{BufReader, Read}, mem, ops, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, string, vec};

use errno::errno;
use libc::{self, AT_FDCWD, fstat, newlocale};

#[repr(C)]
struct file_handle {
    handle_bytes: libc::c_uint,
    handle_type: libc::c_int,
    f_handle: [libc::c_uchar; 0]
}

unsafe extern "C" {
    fn open_by_handle_at(mount_fd: libc::c_int, handle: *const file_handle, flags: libc::c_int) -> libc::c_int;
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Bad fanotify data")]
    BadData,
    #[error("Internal error: {0}")]
    Internal(i32),
    #[error("Error while parsing: {0}")]
    Encoding(#[from] string::FromUtf8Error),
    #[error("Error while probing filesystem: {0}")]
    ReadFS(errno::Errno),
    #[error("Error while fanotify init: {0}")]
    Init(errno::Errno),
    #[error("Error while fanotify mark: {0}")]
    Mark(errno::Errno)
}

#[derive(Debug)]
pub enum Event {
    Create {
        parent_inode: u64,
        inode: u64,
        name: String
    },
    Delete {
        parent_inode: u64,
        inode: u64
    },
    Move {
        old_parent_inode: u64,
        old_name: String,
        new_parent_inode: u64,
        new_name: String,
        inode: u64
    }
}

fn stat_fid(fid: &libc::fanotify_event_info_fid) -> Result<libc::stat, Error> {
    // FIXME: Use the proper filesystem
    // Probably expensive, we should get the fd on startup
    let fd = unsafe {open_by_handle_at(AT_FDCWD, fid.handle.as_ptr() as *const file_handle,
        libc::O_NOATIME
        | libc::O_PATH
    )};
    if fd < 0 {return Err(Error::ReadFS(errno::errno()))}

    let mut stat = mem::MaybeUninit::uninit();
    let err = unsafe {fstat(fd, stat.as_mut_ptr())};
    if err < 0 {return Err(Error::ReadFS(errno::errno()))}
    let stat = unsafe {stat.assume_init()};

    Ok(stat)
}

fn get_name(fid: &libc::fanotify_event_info_fid) -> Result<String, Error> {
    unsafe {
        let handle = fid.handle.as_ptr() as *const file_handle;
        let f_handle = (*handle).f_handle.as_ptr() as *const libc::c_uchar;
        let ptr = f_handle.add((*handle).handle_bytes as usize) as *const libc::c_char;
        let c_str = ffi::CStr::from_ptr(ptr);
        // TODO: Remove unnecessary copy
        Ok(String::from_utf8(c_str.to_bytes().to_vec())?)
    }
}

fn read(buffer: &[u8], ptr: usize) -> (Result<Vec<Event>, Error>, usize) {
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
    dbg!(metadata);
    let next_event = ptr + metadata.event_len as usize;
    let mut ptr = n_ptr;

    return ((|| {

    let mut fid = None;
    let mut dfid = None;
    let mut new_dfid = None;
    let mut old_dfid = None;
    
    while ptr < next_event {
        let (header, _) = read_buffer!(&buffer, ptr, libc::fanotify_event_info_header);
        dbg!(header.info_type);
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
                let msg = read_buffer!(buffer, ptr, libc::fanotify_event_info_error).0;
                return Err(Error::Internal(msg.error))
            }

            libc::FAN_EVENT_INFO_TYPE_DFID
            | libc::FAN_EVENT_INFO_TYPE_PIDFD
            | _ => return Err(Error::BadData)
        }
        ptr += header.len as usize;
    }

    let Some(fid) = fid else {return Err(Error::BadData)};
    let inode = stat_fid(fid)?.st_ino;

    let mut events = vec![];

    if metadata.mask & (libc::FAN_CREATE | libc::FAN_DELETE) != 0 {
        let Some(dfid) = dfid else {return Err(Error::BadData)};

        let parent_inode = stat_fid(dfid)?.st_ino;

        if metadata.mask & libc::FAN_CREATE != 0 {
            events.push(Event::Create {
                parent_inode,
                inode,
                name: get_name(dfid)?
            });
        }
        if metadata.mask & libc::FAN_DELETE != 0 {
            events.push(Event::Delete {
                parent_inode,
                inode
            });
        }
    }

    if metadata.mask & libc::FAN_RENAME != 0 {
        let Some(old_dfid) = old_dfid else {return Err(Error::BadData)};
        let Some(new_dfid) = new_dfid else {return Err(Error::BadData)};

        events.push(Event::Move {
            old_parent_inode: stat_fid(old_dfid)?.st_ino,
            old_name: get_name(old_dfid)?,
            new_parent_inode: stat_fid(new_dfid)?.st_ino,
            new_name: get_name(new_dfid)?,
            inode
        });
    }

    Ok(events)

    })(), next_event)
}

pub fn watch<F>(path: &path::Path, callback: &mut F) -> Result<(), Error>
where F: FnMut(Result<Vec<Event>, Error>) -> ops::ControlFlow<()> {
    let fd = unsafe {libc::fanotify_init(
        libc::FAN_CLASS_NOTIF
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_REPORT_FID
        | libc::FAN_REPORT_NAME
        | libc::FAN_REPORT_TARGET_FID // We might not need this
        | libc::FAN_REPORT_DIR_FID,
        libc::O_LARGEFILE as u32
    )};
    if fd < 0 {return Err(Error::Init(errno::errno()))}

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
    if result < 0 {return Err(Error::Mark(errno::errno()))}

    let mut file = unsafe {fs::File::from_raw_fd(fd)};

    let mut buffer = [0; 4096];

    loop {
        let read_len = file.read(&mut buffer).expect("Failed to read");
        let mut ptr = 0;
        while ptr < read_len {
            let (events, n_ptr) = read(&buffer, ptr);
            ptr = n_ptr;

            match callback(events) {
                ops::ControlFlow::Continue(_) => continue,
                ops::ControlFlow::Break(_) => break
            };
        }
    }
}
