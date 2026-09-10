use std::{ffi::{self, OsStr, OsString}, fs, io, os::fd::AsRawFd, sync::mpsc, thread};


use crate::file_handle::{FileHan, FileHandle, FileHandleOps};

// Walk through discovered directories and generate FAN_OPEN events
// This method ensures consistent ordering between discovery and regular fanotify updates

// If FAN_OPEN generated proper FID and DFID_NAME events we wouldn't need to do all this
// But open()ing a directory generates a DFID_NAME(fid, '.'), so we don't get its parent nor name
// So we manually keep track of what file tree we discovered, and match file handles accordingly
enum Hints {
    // Consumes FAN_OPEN events
    Begin,
    PushChildren(OsString),
    
    // Doesn't consume FAN_OPEN events
    PopChildren,
}

pub struct Reconstructer {
    hints_log: mpsc::Receiver<Hints>,
    stack: Vec<FileHandle>
}

impl Reconstructer {
    pub fn launch(mount_fd: fs::File) -> (Self, mpsc::Sender<FileHandle>, libc::pid_t) {
        let (request_tx, request_rx) = mpsc::channel::<FileHandle>();
        let (hints_tx, hints_rx) = mpsc::channel();
        let (tid_tx, tid_rx) = mpsc::channel();

        thread::spawn(move || {
            tid_tx.send(unsafe {libc::gettid()})
                .expect("Channel broken");
            loop {
                let fh = request_rx.recv()
                    .expect("Channel broken");

                if let Err(e) = walk_fh(&mount_fd, &fh, &hints_tx) {
                    log::warn!("Error while walking: {}", e)
                }
            }
        });

        (Self {
            hints_log: hints_rx,
            stack: vec![]
        }, request_tx, tid_rx.recv().expect("Channel broken"))
    }
    pub fn submit(&mut self, fh: &FileHan) -> Option<(FileHandle, OsString)> {
        loop {
            match self.hints_log.recv().expect("Channel broken") {
                Hints::Begin => {
                    self.stack.push(fh.to_owned());
                    return None
                },
                Hints::PushChildren(name) => {
                    let parent = self.stack.last()
                        .expect("Discover stack bottomed out");
                    let result = (parent.clone(), name);

                    self.stack.push(fh.to_owned());

                    return Some(result)
                },
                Hints::PopChildren => {
                    self.stack.pop().expect("Discover stack bottomed out");
                }
            }
        }
    }
}

fn walk(current: i32, hints_log: &mpsc::Sender<Hints>) -> Result<(), io::Error> {
    let dir_stream = unsafe {libc::fdopendir(current)};

    if dir_stream.is_null() {
        unsafe {libc::close(current)};
        return match io::Error::last_os_error().raw_os_error().unwrap() {
            libc::ENOTDIR => Ok(()),
            _ => Err(io::Error::last_os_error())
        }
    }

    loop {
        let dirent = unsafe {libc::readdir(dir_stream)};
        if dirent.is_null() {break}

        let name = unsafe {ffi::CStr::from_ptr((*dirent).d_name.as_ptr())};
        if name == c"." || name == c".." {continue}

        let child = unsafe {libc::openat(
            current,
            name.as_ptr(),
            libc::O_RDONLY
            | libc::O_NOATIME
            | libc::O_NOFOLLOW
        )};

        if child == -1 {
            match io::Error::last_os_error().raw_os_error().unwrap() {
                libc::ENOENT => continue,
                libc::ELOOP => continue,
                _ => {
                    unsafe {libc::close(current)};
                    return Err(io::Error::last_os_error())
                }
            }
        }

        hints_log.send(Hints::PushChildren(
                unsafe {OsStr::from_encoded_bytes_unchecked(name.to_bytes())}.to_owned()
        )).expect("Channel broken");

        if let Err(e) = walk(child, hints_log) {
            log::warn!("Error while walking {}: {}", name.to_string_lossy(), e);
        }

        hints_log.send(Hints::PopChildren).expect("Channel broken");
    }

    // man closedir(3):
    // A successful call to closedir() also closes the underlying file descriptor
    // associated with dirp
    unsafe {libc::closedir(dir_stream)};
    Ok(())
}

fn walk_fh(mount_fd: &fs::File, fh: &FileHan, hints_log: &mpsc::Sender<Hints>)
-> Result<(), io::Error> {
    let fd = unsafe {libc::open_by_handle_at(
        mount_fd.as_raw_fd(),
        fh.buf().as_ptr() as *mut libc::file_handle,
        libc::O_RDONLY
        | libc::O_NOATIME
        | libc::O_NOFOLLOW
    )};

    if fd == -1 {
        return match io::Error::last_os_error().raw_os_error().unwrap() {
            libc::ESTALE => Ok(()),
            _ => Err(io::Error::last_os_error())
        }
    }

    hints_log.send(Hints::Begin).expect("Channel broken");

    walk(fd, hints_log)
}
