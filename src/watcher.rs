use std::{ffi::{self, OsStr, OsString}, fs, io, os::{fd::FromRawFd, unix::ffi::OsStrExt}, path, sync::mpsc, thread, time};

use crate::{GlobalArgs, db, fanotify_reader, file_handle::FileHandle, watchpath};

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
    Move {
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
            Event::Move { old_p_fh, new_p_fh, fh, old_name, new_name } => {
                handle_move(&tx, old_p_fh, new_p_fh, fh, old_name, new_name)
            }
        }?
    }

    db::map_db_err(tx.commit())?;

    Ok(())
}

fn fanotify_thread(
    path: path::PathBuf, filter: watchpath::Filter,
    tx: mpsc::Sender<Vec<Event>>
) -> anyhow::Result<()> {
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
        return Err(
            anyhow::anyhow!(io::Error::last_os_error())
                .context("Failed to fanotify_init")
        );
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
        return Err(
            anyhow::anyhow!(io::Error::last_os_error())
                .context("Failed to fanotify_mark")
        );
    }

    let file = unsafe {fs::File::from_raw_fd(fd)};
    let mut reader = fanotify_reader::Reader::new(file);

    let mut events = vec![];

    loop {
        events.clear();

        if let Err(e) = reader.map_events(&mut |x| {
            if !filter.apply(&x.fh()?) {return Ok(())}

            if let Some(e) = x.as_create()? {
                events.push(Event::Create {
                    p_fh: e.p_fh()?,
                    fh: x.fh()?,
                    name: e.name()?.to_os_string()
                });
            }

            if let Some(e) = x.as_delete()? {
                events.push(Event::Delete {
                    p_fh: e.p_fh()?,
                    fh: x.fh()?,
                    name: e.name()?.to_os_string()
                });
            }

            if let Some(e) = x.as_move()? {
                events.push(Event::Move {
                    old_p_fh: e.old_p_fh()?,
                    new_p_fh: e.new_p_fh()?,
                    fh: x.fh()?,
                    old_name: e.old_name()?.to_os_string(),
                    new_name: e.new_name()?.to_os_string()
                });
            }

            Ok(())
        }) {
            log::warn!("Fanotify thread error: {e}")
        }
        if events.len() == 0 {continue}
        tx.send(events.clone()).unwrap();
    }
}

fn watch(path: &path::Path, filter: watchpath::Filter, conn: &mut rusqlite::Connection)
-> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();

    let path_buf = path.to_path_buf();

    let _watch_thread = thread::spawn(move || {
        if let Err(e) = fanotify_thread(path_buf, filter, tx) {
            panic!("{:?}", e);
        }
    });

    let debounce_duration = time::Duration::from_millis(100);
    let mut debounce_queue = vec![];

    loop {
        let first = rx.recv().unwrap();

        debounce_queue.extend_from_slice(&first);

        loop {
            let msg = match rx.recv_timeout(debounce_duration) {
                Ok(x) => x,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                _ => panic!()
            };

            debounce_queue.extend_from_slice(&msg);
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

    db::prepare_db(&conn)?;

    let (path, filter) = watchpath::prepare_fanotify(&args.watch_path)?;

    watch(&path, filter, &mut conn)?;
    Ok(())
}
