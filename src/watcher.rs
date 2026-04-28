use std::{ops, path, sync::mpsc, thread, time};


use crate::{db, fanotify};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Messaging error")]
    Messaging,
    #[error(transparent)]
    Fanotify(#[from] fanotify::Error),
    #[error(transparent)]
    DB(#[from] db::Error)
}

#[derive(Debug)]
enum Message {
    Error(Error),
    Events(Vec<fanotify::Event>)
}

fn handle_message(tx: &mpsc::Sender<Message>,
    msg: Result<Vec<fanotify::Event>, fanotify::Error>)
-> Result<(), Error> {
    let msg = match msg {
        Ok(x) => x,
        Err(e) => {
            log::warn!("fanotify error: {}", e);
            return Ok(())
        }
    };

    tx.send(Message::Events(msg))
        .map_err(|_| Error::Messaging)?;

    Ok(())
}

fn get_parent_id(tx: &rusqlite::Transaction, p_fh: &[u8], name: &str)
-> Result<i64, Error> {
        let id = db::get_single_id(&tx, p_fh);
        if let Err(db::Error::NoFile) = id {
            log::warn!("Failed to find parent of file: {name}")
        }
        Ok(id?)
}

fn handle_create(tx: &rusqlite::Transaction, p_fh: &[u8], fh: &[u8], name: &str)
-> Result<(), Error> {
    let parent_id = get_parent_id(tx, p_fh, name)?;

    match db::create(tx, parent_id, fh, name) {
        Ok(x) => Ok(x),
        Err(db::Error::DuplicateFile) => {
            log::warn!("File `{name}` already exists at location, overwriting");
            db::delete(tx, db::get_rough_id(tx, parent_id, name)?)?;
            db::create(tx, parent_id, fh, name)?;
            return Ok(())
        }
        Err(e) => Err(e)
    }?;
    Ok(())
}

fn handle_delete(tx: &rusqlite::Transaction, p_fh: &[u8], fh: &[u8], name: &str)
-> Result<(), Error> {
    let parent_id = get_parent_id(tx, p_fh, name)?;

    let id = match db::get_id(tx, parent_id, fh, name) {
        Ok(x) => Ok(x),
        Err(db::Error::NoFile) => {
            log::warn!("Attempted to delete file `{name}` that doesn't exist");
            return Ok(())
        }
        Err(e) => Err(e)
    }?;

    db::delete(tx, id)?;
    Ok(())
}

fn handle_move(
    tx: &rusqlite::Transaction,
    old_p_fh: &[u8], new_p_fh: &[u8], fh: &[u8], old_name: &str, new_name: &str
) -> Result<(), Error> {
    let old_parent_id = get_parent_id(tx, old_p_fh, old_name)?;
    let new_parent_id = get_parent_id(tx, new_p_fh, new_name)?;

    let id = match db::get_id(tx, old_parent_id, fh, old_name) {
        Ok(x) => Ok(x),
        Err(db::Error::NoFile) => {
            log::warn!("Attempted to move file `{old_name}` that doesn't exist. \
                creating new file.");
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

pub fn watch(path: &path::Path, conn: &mut rusqlite::Connection) -> Result<(), Error> {
    let (tx, rx) = mpsc::channel();

    let path_buf = path.to_path_buf();

    let _watch_thread = thread::spawn(move || {
        let mut result = Ok(());

        let e = fanotify::watch(&path_buf, &mut |x| {
            handle_message(&tx, x)
                .map_or_else(|e| {
                    result = Err(e);
                    ops::ControlFlow::Break(())
                }, |_| ops::ControlFlow::Continue(()))
        });

        let _ = e.map_err(|e| tx.send(Message::Error(e.into()))
            .expect("Failed to send error; panicking"));
        let _ = result.map_err(|e| tx.send(Message::Error(e))
            .expect("Failed to send error; panicking"));
    });

    let debounce_duration = time::Duration::from_millis(100);
    let mut debounce_queue = vec![];

    loop {
        let first = rx.recv()
            .map_err(|_| Error::Messaging)?;
        let first = match first {
            Message::Error(e) => return Err(e),
            Message::Events(x) => x
        };
        debounce_queue.extend_from_slice(&first);

        loop {
            let msg = match rx.recv_timeout(debounce_duration) {
                Ok(x) => x,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected)
                    => return Err(Error::Messaging),
            };
            let msg = match msg {
                Message::Error(e) => return Err(e),
                Message::Events(x) => x
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
