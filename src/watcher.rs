use std::{ffi::OsStr, ops, path, sync::mpsc, thread, time};

use crate::{GlobalArgs, db, fanotify, file_handle::FileHandle, watchpath};

#[derive(clap::Args, Debug, Clone)]
pub struct WatchArgs {
    #[command(flatten)]
    watch_path: watchpath::WatchPathArgs
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Fanotify(#[from] fanotify::Error),
    #[error(transparent)]
    DB(#[from] db::Error)
}

fn get_parent_id(tx: &rusqlite::Transaction, p_fh: &FileHandle, name: &OsStr)
-> Result<i64, Error> {
        let id = db::get_single_id(&tx, p_fh);
        if let Err(db::Error::NoFile) = id {
            log::warn!("Failed to find parent of file: {}", name.display())
        }
        Ok(id?)
}

fn handle_create(tx: &rusqlite::Transaction, p_fh: &FileHandle, fh: &FileHandle, name: &OsStr)
-> Result<(), Error> {
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
-> Result<(), Error> {
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
) -> Result<(), Error> {
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

fn handle_events(conn: &mut rusqlite::Connection, events: &Vec<fanotify::Event>)
-> Result<(), Error> {
    let tx = db::map_db_err(conn.transaction())?;

    for event in events {
        match event {
            fanotify::Event::Create {p_fh, fh, name} => {
                handle_create(&tx, p_fh, fh, name)
            }
            fanotify::Event::Delete { p_fh, fh, name } => {
                handle_delete(&tx, p_fh, fh, name)
            }
            fanotify::Event::Move { old_p_fh, new_p_fh, fh, old_name, new_name } => {
                handle_move(&tx, old_p_fh, new_p_fh, fh, old_name, new_name)
            }
        }?
    }

    db::map_db_err(tx.commit())?;

    Ok(())
}

fn watch(path: &path::Path, filter: watchpath::Filter, conn: &mut rusqlite::Connection)
-> Result<(), Error> {
    let (tx, rx) = mpsc::channel();

    let path_buf = path.to_path_buf();

    let _watch_thread = thread::spawn(move || {
        fanotify::watch(&path_buf, &filter, &mut |x| {
            tx.send(x.clone()).unwrap();
            ops::ControlFlow::Continue(())
        }).unwrap()
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
