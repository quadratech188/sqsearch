use std::{ffi::{self, OsStr, OsString}, fs, io::{self, Read}, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, sync::mpsc, thread, time};

use crate::{GlobalArgs, db, fanotify, file_handle::{FileHan, FileHandle}, watchpath};

#[derive(clap::Args, Debug, Clone)]
pub struct WatchArgs {
    #[command(flatten)]
    watch_path: watchpath::WatchPathArgs
}

#[derive(Clone, Debug)]
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
    Rename {
        old_p_fh: FileHandle,
        new_p_fh: FileHandle,
        fh: FileHandle,
        old_name: OsString,
        new_name: OsString,
    }
}

// Reporting principles:
// - If we didn't know about the parent, don't report anything
// - If we did know, report everything

fn handle_move(
    tx: &rusqlite::Transaction,
    fh: &FileHan,
    from: Option<(&OsStr, &FileHan)>, to: Option<(&OsStr, &FileHan)>
) -> Result<(), db::Error> {
    // If we can't find the parent, pretend it doesn't exist (None)
    let get_ids = |(name, fh)| {
        match db::get_dir_id(tx, fh) {
            Ok(id) => Ok(Some((name, id))),
            Err(db::Error::NoFile) => Ok(None),
            Err(e) => Err(e)
        }
    };

    let mut from = from.map_or(Ok(None), get_ids)?;
    let to = to.map_or(Ok(None), get_ids)?;

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

                log::warn!(
                    "Destination entry conflict: parent_id={}, name={}; overwriting",
                    p_id, name.display()
                );
                db::delete_with_id(tx, db::get_dirent_id(tx, name, p_id)?)?;
            }
            Err(e) => return Err(e)
        }
    }
    Ok(())
}

fn handle_events(conn: &mut rusqlite::Connection, events: &[Event])
-> Result<(), db::Error> {
    let tx = conn.transaction()?;

    for event in events {
        match event {
            Event::Create {p_fh, fh, name} => {
                handle_move(
                    &tx, fh,
                    None,
                    Some((name, p_fh))
                )
            }
            Event::Delete { p_fh, fh, name } => {
                handle_move(
                    &tx, fh,
                    Some((name, p_fh)),
                    None
                )
            }
            Event::Rename { old_p_fh, new_p_fh, fh, old_name, new_name } => {
                handle_move(
                    &tx, fh,
                    Some((old_name, old_p_fh)),
                    Some((new_name, new_p_fh)),
                )
            }
        }?
    }

    tx.commit()?;
    Ok(())
}

fn create_fanotify_stream(path: &path::Path) -> anyhow::Result<fs::File> {
    let fd = unsafe {libc::fanotify_init(
        libc::FAN_CLASS_NOTIF
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_REPORT_FID
        | libc::FAN_REPORT_NAME
        | libc::FAN_REPORT_TARGET_FID
        | libc::FAN_REPORT_DIR_FID,

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
        | libc::FAN_RENAME,

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

fn process_fanotify_stream(
    mut stream: fs::File, filter: watchpath::Filter, tx: mpsc::Sender<Event>
) -> anyhow::Result<()> {

    let mut buf = [0; 4096];
    let mut ptr = 0;
    let mut len = 0;
    loop {
        if ptr >= len {
            len = stream.read(&mut buf)?;
            ptr = 0;
        }

        let (event, len) = fanotify::Event::from_slice(&buf[ptr..]);
        ptr += len;

        if !filter.apply(event.fh()) {continue}

        tx.send(match event.r#type {
            fanotify::EventType::Create(dfid) => Event::Create {
                p_fh: event.p_fh(dfid).to_owned(),
                fh:   event.fh()      .to_owned(),
                name: event.name(dfid).to_owned()
            },
            fanotify::EventType::Delete(dfid) => Event::Delete {
                p_fh: event.p_fh(dfid).to_owned(),
                fh:   event.fh()      .to_owned(),
                name: event.name(dfid).to_owned()
            },
            fanotify::EventType::Rename(old_dfid, new_dfid) => Event::Rename {
                old_p_fh: event.p_fh(old_dfid).to_owned(),
                new_p_fh: event.p_fh(new_dfid).to_owned(),
                fh:       event.fh()          .to_owned(),
                old_name: event.name(old_dfid).to_owned(),
                new_name: event.name(new_dfid).to_owned()
            },
            fanotify::EventType::CreateDelete(_) => continue
        })?;
    }
}

fn watch(path: &path::Path, filter: watchpath::Filter, conn: &mut rusqlite::Connection)
-> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let fanotify_stream = create_fanotify_stream(path)?;

    let _watch_thread = thread::spawn(|| {
        if let Err(e) = process_fanotify_stream(fanotify_stream, filter, tx) {
            panic!("{:?}", e)
        }
    });

    let debounce_duration = time::Duration::from_millis(100);
    let mut debounce_queue = vec![];

    loop {
        let first = rx.recv().unwrap();

        debounce_queue.push(first);

        loop {
            let msg = match rx.recv_timeout(debounce_duration) {
                Ok(x) => x,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                _ => panic!()
            };

            debounce_queue.push(msg);
        }

        log::debug!("Writing {} event(s) to DB", debounce_queue.len());
        match handle_events(conn, &debounce_queue) {
            Ok(()) => log::debug!("Wrote {} event(s) to DB", debounce_queue.len()),
            Err(e) => log::warn!("{}", e)
        }
        debounce_queue.clear();
    }
}

pub fn exec(globals: &GlobalArgs, args: &WatchArgs) -> anyhow::Result<()> {
    let mut conn = rusqlite::Connection::open_with_flags(
        &globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    )?;

    db::prepare_db(&mut conn)?;

    let (path, filter) = watchpath::prepare_fanotify(&args.watch_path)?;

    watch(&path, filter, &mut conn)?;
    Ok(())
}
