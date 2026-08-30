use std::{ffi::{self, OsStr, OsString}, fs, io::{self, Read}, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, sync::mpsc, thread, time};

use crate::{GlobalArgs, db, fanotify, file_handle::FileHandle, watchpath};

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

fn get_parent_id(tx: &rusqlite::Transaction, p_fh: &FileHandle, name: &OsStr)
-> Result<i64, db::Error> {
        let id = db::get_single_id(&tx, p_fh);
        if let Err(db::Error::NoFile) = id {
            log::warn!("Failed to find parent of file: {}", name.display())
        }
        Ok(id?)
}

fn handle_create(tx: &rusqlite::Transaction, p_fh: &FileHandle, fh: &FileHandle, name: &OsStr)
-> Result<(), db::Error> {
    let parent_id = get_parent_id(tx, p_fh, name)?;

    match db::create(tx, parent_id, fh, name) {
        Ok(x) => Ok(x),
        Err(db::Error::DuplicateFile) => {
            log::warn!(
                "File `{}` already exists at location, overwriting",
                name.display()
            );
            db::delete(tx, db::get_rough_id(tx, parent_id, name)?)?;
            db::create(tx, parent_id, fh, name)?;
            return Ok(())
        }
        Err(e) => Err(e)
    }?;
    Ok(())
}

fn handle_delete(tx: &rusqlite::Transaction, p_fh: &FileHandle, fh: &FileHandle, name: &OsStr)
-> Result<(), db::Error> {
    let parent_id = get_parent_id(tx, p_fh, name)?;

    let id = match db::get_id(tx, parent_id, fh, name) {
        Ok(x) => Ok(x),
        Err(db::Error::NoFile) => {
            log::warn!(
                "Attempted to delete file `{}` that doesn't exist", 
                name.display()
            );
            return Ok(())
        }
        Err(e) => Err(e)
    }?;

    db::delete(tx, id)?;
    Ok(())
}

fn handle_move(
    tx: &rusqlite::Transaction,
    old_p_fh: &FileHandle, new_p_fh: &FileHandle, fh: &FileHandle, old_name: &OsStr, new_name: &OsStr
) -> Result<(), db::Error> {
    let old_parent_id = get_parent_id(tx, old_p_fh, old_name)?;
    let new_parent_id = get_parent_id(tx, new_p_fh, new_name)?;

    let id = match db::get_id(tx, old_parent_id, fh, old_name) {
        Ok(x) => Ok(x),
        Err(db::Error::NoFile) => {
            log::warn!(
                "Attempted to move file `{}` that doesn't exist. creating new file.",
                old_name.display()
            );
            db::create(tx, new_parent_id, fh, new_name)?;
            return Ok(())
        }
        Err(e) => Err(e)
    }?;

    match db::r#move(tx, id, new_parent_id, new_name) {
        Ok(x) => Ok(x),
        Err(db::Error::DuplicateFile) => {
            // touch a; touch b; mv a b
            db::delete(tx, db::get_rough_id(tx, new_parent_id, new_name)?)?;
            db::r#move(tx, id, new_parent_id, new_name)?;
            return Ok(())
        }
        Err(e) => Err(e)
    }?;
    Ok(())
}

fn handle_events(conn: &mut rusqlite::Connection, events: &[Event])
-> Result<(), db::Error> {
    let tx = db::map_db_err(conn.transaction())?;

    for event in events {
        match event {
            Event::Create {p_fh, fh, name} => {
                handle_create(&tx, p_fh, fh, name)
            }
            Event::Delete { p_fh, fh, name } => {
                handle_delete(&tx, p_fh, fh, name)
            }
            Event::Rename { old_p_fh, new_p_fh, fh, old_name, new_name } => {
                handle_move(&tx, old_p_fh, new_p_fh, fh, old_name, new_name)
            }
        }?
    }

    db::map_db_err(tx.commit())?;

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
