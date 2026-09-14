use std::{ffi::{self, OsStr, OsString}, fs, io::{self, Read}, ops::Deref, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, sync::mpsc, thread, time};

use anyhow::Context;

use crate::{GlobalArgs, db, discoverer, fanotify, file_handle::{FileHan, FileHandle}, filter};

#[derive(clap::Args, Debug, Clone)]
pub struct WatchArgs {
    /// Any path within the filesystem / subvolume you want to watch
    path: path::PathBuf,

    /// Path to filters file
    #[arg(long, default_value = "/etc/sqsearch-filters.conf")]
    filters: path::PathBuf,

    /// If watching a BTRFS subvolume, mount point of the root subvolume.
    #[arg(long)]
    btrfs_root: Option<path::PathBuf>
}

#[derive(Clone, Debug)]
pub enum Event {
    Alter {
        old_dirent: Option<(FileHandle, OsString)>,
        new_dirent: Option<(FileHandle, OsString)>,
        fh: FileHandle,
        is_dir: bool
    },
    Discover {
        p_fh: FileHandle,
        fh: FileHandle,
        name: OsString
    }
}

// Reporting principles:
// - If we didn't know about the parent, don't report anything
// - If we did know, report everything

fn handle_move(
    tx: &rusqlite::Transaction, discover_tx: &mpsc::Sender<FileHandle>,
    fh: &FileHan,
    from: Option<(&OsStr, &FileHan)>, to: Option<(&OsStr, &FileHan)>,
    is_dir: bool
) -> Result<(), db::Error> {
    // If we can't find the parent, pretend it doesn't exist (None)
    let get_ids = |(name, fh)| {
        match db::get_dir_id(tx, fh) {
            Ok(id) => Ok(Some((name, id))),
            Err(db::Error::NoFile) => Ok(None),
            Err(e) => Err(e)
        }
    };

    let from_id = from.map_or(Ok(None), get_ids)?;
    let to_id = to.map_or(Ok(None), get_ids)?;

    if is_dir && let (Some(_), None, Some(_), Some((name, parent_id))) = (from, from_id, to, to_id) {
        log::debug!(
            "Queue discover for new directory: parent_id={}, name={}",
            parent_id, name.display()
        );
        discover_tx.send(fh.to_owned())
            .expect("Channel broken");
    }

    let mut from = from_id;
    let to = to_id;

    loop {
        let result = match (from, to) {
            (Some((old_name, old_p_id)), Some((new_name, new_p_id)))
                => db::update(tx, fh, old_name, old_p_id, new_name, new_p_id),
            (Some((name, p_id)), None)
                => db::delete(tx, fh, name, p_id),
            (None, Some((name, p_id)))
                => db::create(tx, fh, name, p_id).map(|_| ()),
            (None, None) => break
        };

        match result {
            Ok(()) => return Ok(()),
            Err(db::Error::NoFile) => {
                let (name, p_id) = from.unwrap();

                log::warn!(
                    "Source entry missing: parent_id={}, name={}; ignoring",
                    p_id, name.display()
                );
                from = None;
            },
            Err(db::Error::NameTaken) => {
                let (name, p_id) = to.unwrap();

                // The below behavior isn't reliable anymore.
                // `from` could be None because we didn't know about it, or it was ignored.
                // We choose to not report at all.

                /*
                if let None = from {
                    // touch a; touch b; mv a b
                    // This is normal behavior for updates, so don't print the warning
                    log::warn!(
                        "Destination entry conflict: parent_id={}, name={}; overwriting",
                        p_id, name.display()
                    );
                }
                */

                db::delete_with_id(tx, db::get_dirent_id(tx, name, p_id)?)?;
            }
            Err(e) => return Err(e)
        }
    }
    Ok(())
}

fn handle_discover(tx: &rusqlite::Transaction, p_fh: &FileHan, fh: &FileHan, name: &OsStr)
-> Result<(), db::Error> {
    let p_id = db::get_dir_id(tx, p_fh)?;

    match db::create(tx, fh, name, p_id) {
        Ok(_) => Ok(()),
        Err(db::Error::NameTaken) => Ok(()),
        Err(e) => Err(e)
    }
}

struct DbState {
    conn: rusqlite::Connection,
    fanotify_rx: mpsc::Receiver<Event>,
    discover_tx: mpsc::Sender<FileHandle>
}

impl DbState {
    fn handle_events(&mut self, events: &[Event]) -> Result<(), db::Error> {
        let tx = self.conn.transaction()?;

        for event in events {
            match event {
                Event::Alter { old_dirent, new_dirent, fh, is_dir } => {
                    handle_move(
                        &tx,
                        &self.discover_tx,
                        &fh,
                        old_dirent.as_ref().map(|x| (x.1.deref(), x.0.deref())),
                        new_dirent.as_ref().map(|x| (x.1.deref(), x.0.deref())),
                        *is_dir
                    )
                }
                Event::Discover { p_fh, fh, name } => {
                    handle_discover(&tx, p_fh, fh, name)
                }
            }?
        }

        tx.commit()?;
        Ok(())
    }
    fn run(&mut self) -> Result<(), db::Error> {
        let debounce_duration = time::Duration::from_millis(100);
        let mut debounce_queue = vec![];

        loop {
            let first = self.fanotify_rx.recv().unwrap();

            debounce_queue.push(first);

            loop {
                let msg = match self.fanotify_rx.recv_timeout(debounce_duration) {
                    Ok(x) => x,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    _ => panic!()
                };

                debounce_queue.push(msg);
            }

            log::debug!("Writing {} event(s) to DB", debounce_queue.len());
            match self.handle_events(&debounce_queue) {
                Ok(()) => log::debug!("Wrote {} event(s) to DB", debounce_queue.len()),
                Err(e) => log::warn!("{}", e)
            }
            debounce_queue.clear();
        }
    }
}

fn create_fanotify_stream(path: &path::Path) -> anyhow::Result<fs::File> {
    let fd = unsafe {libc::fanotify_init(
        libc::FAN_CLASS_NOTIF
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_REPORT_FID
        | libc::FAN_REPORT_NAME
        | libc::FAN_REPORT_TARGET_FID
        | libc::FAN_REPORT_DIR_FID
        | libc::FAN_REPORT_TID,

        libc::O_LARGEFILE as u32
    )};

    if fd < 0 {anyhow::bail!(
        "Failed to fanotify_init: {}",
        io::Error::last_os_error().to_string()
    )}

    let err = unsafe {libc::fanotify_mark(fd,
        libc::FAN_MARK_ADD
        | libc::FAN_MARK_FILESYSTEM,

        libc::FAN_ONDIR
        | libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_RENAME
        | libc::FAN_OPEN,

        libc::AT_FDCWD,

        ffi::CString::new(path.as_os_str().as_bytes())
            .unwrap()
            .as_ptr()
    )};

    if err < 0 {anyhow::bail!(
        "Failed to fanotify_mark: {}",
        io::Error::last_os_error().to_string()
    )}

    Ok(unsafe {fs::File::from_raw_fd(fd)})
}

struct FanotifyState {
    stream: fs::File,
    filter: filter::Filter,
    fanotify_tx: mpsc::Sender<Event>,

    reconstructer: discoverer::Reconstructer,
    discover_tid: libc::pid_t
}

impl FanotifyState {
    fn run(&mut self) -> Result<(), io::Error> {
        let mut buf = [0; 1024 * 1024];
        let mut ptr = 0;
        let mut len = 0;
        loop {
            if ptr >= len {
                len = self.stream.read(&mut buf)?;
                ptr = 0;
            }

            let here = &buf[ptr..];

            let metadata = fanotify::read_metadata(here);
            ptr += metadata.event_len as usize;

            let stripped_mask = metadata.mask
                & (libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_RENAME | libc::FAN_OPEN);

            // Only allow FAN_OPENs by our discoverer
            if stripped_mask == libc::FAN_OPEN && metadata.pid != self.discover_tid {continue}

            // This has net zero effect
            if stripped_mask == libc::FAN_CREATE | libc::FAN_DELETE {continue}

            if stripped_mask & libc::FAN_CREATE != 0 {
                let (fh, new)
                    = fanotify::read_create_delete(here, &metadata);

                if !self.filter.allow(fh, Some(new)) {continue}

                self.fanotify_tx.send(Event::Alter {
                    old_dirent: None,
                    new_dirent: Some((new.0.to_owned(), new.1.to_owned())),
                    fh: fh.to_owned(),
                    is_dir: (metadata.mask & libc::FAN_ONDIR != 0)
                }).expect("Channel broken");
            }
            if stripped_mask & libc::FAN_DELETE != 0 {
                let (fh, old)
                    = fanotify::read_create_delete(here, &metadata);

                if !self.filter.allow(fh, Some(old)) {continue}

                self.fanotify_tx.send(Event::Alter {
                    old_dirent: Some((old.0.to_owned(), old.1.to_owned())),
                    new_dirent: None,
                    fh: fh.to_owned(),
                    is_dir: (metadata.mask & libc::FAN_ONDIR != 0)
                }).expect("Channel broken");
            }
            if stripped_mask & libc::FAN_RENAME != 0 {
                let (fh, old, new)
                    = fanotify::read_rename(here, &metadata);

                let mut old = Some(old);
                let mut new = Some(new);
                if !self.filter.allow(fh, old) {old = None}
                if !self.filter.allow(fh, new) {new = None}

                if let None = old && let None = new {continue}

                self.fanotify_tx.send(Event::Alter {
                    old_dirent: old.map(|x| (x.0.to_owned(), x.1.to_owned())),
                    new_dirent: new.map(|x| (x.0.to_owned(), x.1.to_owned())),
                    fh: fh.to_owned(),
                    is_dir: (metadata.mask & libc::FAN_ONDIR != 0)
                }).expect("Channel broken");
            }
            if stripped_mask == libc::FAN_OPEN {
                let fh = fanotify::read_open(here, &metadata);

                let Some(new) = self.reconstructer.submit(fh) else {continue};

                if !self.filter.allow(fh, Some((&new.0, &new.1))) {continue}

                self.fanotify_tx.send(Event::Discover {
                    p_fh: new.0,
                    fh: fh.to_owned(),
                    name: new.1,
                }).expect("Channel broken");
            }
        }
    }
}

pub fn exec(globals: &GlobalArgs, args: &WatchArgs) -> anyhow::Result<()> {
    let mut filter = filter::Filter::load(&args.filters)
        .context("Failed to load config file")?;
    if let Some(_) = &args.btrfs_root {
        filter.add_btrfs_subvol(&args.path)?;
    }

    let fanotify_path = args.btrfs_root.as_ref().unwrap_or(&args.path);
    let mount_fd = fs::File::open(fanotify_path)?;
    let stream = create_fanotify_stream(fanotify_path)?;

    let mut conn = rusqlite::Connection::open_with_flags(
        &globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    )?;
    db::prepare_db(&mut conn)?;

    let (fanotify_tx, fanotify_rx) = mpsc::channel();

    let (reconstructer, discover_tx, discover_tid)
        = discoverer::Reconstructer::launch(mount_fd);

    let mut fanotify_state = FanotifyState {
        stream,
        filter,
        fanotify_tx,
        reconstructer,
        discover_tid
    };

    let _ = thread::spawn(move || {
        if let Err(e) = fanotify_state.run() {
            panic!("{:?}", e)
        }
    });

    let mut db_state = DbState {
        conn,
        fanotify_rx,
        discover_tx
    };

    db_state.run()?;
    Ok(())
}
