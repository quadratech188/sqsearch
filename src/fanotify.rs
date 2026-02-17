use std::{ffi, fs, io::Read, mem, ops, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, string, vec};

use libc;

use crate::db;

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
    #[error("Bad path: {0}")]
    BadPath(path::PathBuf),
    #[error("Bad fanotify data")]
    BadData,
    #[error("From fanotify: {0}")]
    Internal(i32),
    #[error("While parsing: {0}")]
    Encoding(#[from] string::FromUtf8Error),
    #[error("While probing filesystem: {0}")]
    ReadFS(errno::Errno),
    #[error("While fanotify init: {0}")]
    Init(errno::Errno),
    #[error("While fanotify mark: {0}")]
    Mark(errno::Errno)
}

// FIXME: Use CString instead of String

#[derive(Debug, Clone)]
pub enum Event {
    Create {
        parent_inode: u64,
        inode: u64,
        name: String
    },
    Delete {
        parent_inode: u64,
        name: String
    },
    Move {
        old_parent_inode: u64,
        old_name: String,
        new_parent_inode: u64,
        new_name: String,
        inode: u64
    }
}

fn get_mount_fd(path: &path::Path) -> Result<libc::c_int, Error> {
    let fp = unsafe {libc::setmntent(c"/etc/mtab".as_ptr(), c"r".as_ptr())};

    let path = loop {
        let mntent = unsafe {libc::getmntent(fp)};
        if mntent.is_null() {break Err(Error::BadPath(path.into()))}
        let mntent = unsafe {*mntent};

        let dir = ffi::OsStr::from_bytes(unsafe {
                ffi::CStr::from_ptr(mntent.mnt_dir)
            }.to_bytes());

        if dir == path.as_os_str() {break Ok(mntent.mnt_dir)}
    }?;

    let fd = unsafe {libc::open(path, libc::O_NOATIME, libc::O_PATH)};

    unsafe {libc::endmntent(fp)};

    Ok(fd)
}

fn stat_fid(mount_fd: libc::c_int, fid: &libc::fanotify_event_info_fid) -> Result<libc::stat, Error> {
    let fd = unsafe {open_by_handle_at(mount_fd, fid.handle.as_ptr() as *const file_handle,
        libc::O_NOATIME
        | libc::O_PATH
    )};
    if fd < 0 {return Err(Error::ReadFS(errno::errno()))}

    let mut stat = mem::MaybeUninit::uninit();
    let err = unsafe {libc::fstat(fd, stat.as_mut_ptr())};
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

fn read(mount_fd: libc::c_int, buffer: &[u8], ptr: usize) -> (Result<Vec<Event>, Error>, usize) {
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

    return ((|| {

    let mut fid = None;
    let mut dfid = None;
    let mut new_dfid = None;
    let mut old_dfid = None;
    
    while ptr < next_event {
        let (header, _) = read_buffer!(&buffer, ptr, libc::fanotify_event_info_header);
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

    let mut events = vec![];

    if metadata.mask & (libc::FAN_CREATE | libc::FAN_DELETE) != 0 {
        let Some(dfid) = dfid else {return Err(Error::BadData)};

        if metadata.mask & libc::FAN_CREATE != 0 {
            let stat = match stat_fid(mount_fd, fid) {
                Ok(x) => x,
                // The file was already deleted, we don't care about it
                Err(Error::ReadFS(errno::Errno(libc::ESTALE))) => return Ok(events),
                Err(e) => return Err(e)
            };

            events.push(Event::Create {
                parent_inode: stat_fid(mount_fd, dfid)?.st_ino,
                inode: stat.st_ino,
                name: get_name(dfid)?
            });
        }
        // FIXME: Recursively delete files if the parent cannot be found
        if metadata.mask & libc::FAN_DELETE != 0 {
            events.push(Event::Delete {
                parent_inode: stat_fid(mount_fd, dfid)?.st_ino,
                name: get_name(dfid)?
            });
        }
    }

    if metadata.mask & libc::FAN_RENAME != 0 {
        let Some(old_dfid) = old_dfid else {return Err(Error::BadData)};
        let Some(new_dfid) = new_dfid else {return Err(Error::BadData)};
        
        // FIXME: We can't track files moved out of then-deleted directories

        let stat = match stat_fid(mount_fd, fid) {
            Ok(x) => x,
            // The file was already deleted, we don't care about it
            Err(Error::ReadFS(errno::Errno(libc::ESTALE))) => return Ok(events),
            Err(e) => return Err(e)
        };

        events.push(Event::Move {
            old_parent_inode: stat_fid(mount_fd, old_dfid)?.st_ino,
            old_name: get_name(old_dfid)?,
            new_parent_inode: stat_fid(mount_fd, new_dfid)?.st_ino,
            new_name: get_name(new_dfid)?,
            inode: stat.st_ino
        });
    }

    Ok(events)

    })(), next_event)
}

pub fn watch<F>(path: &path::Path, callback: &mut F) -> Result<(), Error>
where F: FnMut(Result<Vec<Event>, Error>) -> ops::ControlFlow<()> {

    let mount_fd = get_mount_fd(path)?;

    let fd = unsafe {libc::fanotify_init(
        libc::FAN_CLASS_NOTIF
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_REPORT_FID
        | libc::FAN_REPORT_NAME
        | libc::FAN_REPORT_TARGET_FID
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

    log::info!("Watching {}", path.display());

    let mut file = unsafe {fs::File::from_raw_fd(fd)};

    let mut buffer = [0; 4096];

    loop {
        let read_len = file.read(&mut buffer).expect("Failed to read");
        let mut ptr = 0;
        while ptr < read_len {
            let (events, n_ptr) = read(mount_fd, &buffer, ptr);
            ptr = n_ptr;

            match callback(events) {
                ops::ControlFlow::Continue(_) => continue,
                ops::ControlFlow::Break(_) => break
            };
        }
    }
}
