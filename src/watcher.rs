use std::{ops, path, sync::mpsc, thread, time};

use libc::time;

use crate::{db, fanotify};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Messaging error")]
    Messaging,
    #[error("Not a filesystem mount: {0}")]
    BadPath(path::PathBuf),
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

fn handle_events(conn: &mut rusqlite::Connection, events: &Vec<fanotify::Event>)
-> Result<(), Error> {
    let tx = db::map_db_err(conn.transaction())?;

    for event in events {
        match event {
            fanotify::Event::Create { parent_inode, inode, name } => {
                match db::create(&tx, *parent_inode, *inode, name) {
                    Err(db::Error::DuplicateFile(_, _, _)) => {
                        log::warn!("Attempted to create file that already \
                            existed in DB: `{}`", name);
                        Ok(())
                    },
                    x => x
                }?
            }
            fanotify::Event::Delete { parent_inode, name } => {
                match db::delete(&tx, *parent_inode, name) {
                    Err(db::Error::NoFile(_, _)) => {
                        log::warn!("Attempted to delete file that doesn't \
                            exist in DB: `{}`", name);
                        Ok(())
                    },
                    x => x
                }?
            }
            fanotify::Event::Move { old_parent_inode, old_name,
                new_parent_inode, new_name, inode } => {
                match db::r#move(&tx, *old_parent_inode, old_name, *new_parent_inode,
                    new_name, *inode) {
                    Err(db::Error::NoFile(_, _)) => {
                        log::warn!("Attempted to move file that doesn't \
                            exist in DB: `{}`, Creating new file.", old_name);
                        db::create(&tx, *new_parent_inode, *inode, new_name)
                    }
                    x => x
                }?
            }
        }
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
