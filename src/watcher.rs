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

fn handle_events(conn: &mut rusqlite::Connection, events: &Vec<fanotify::Event>)
-> Result<(), Error> {
    let tx = db::map_db_err(conn.transaction())?;

    for event in events {
        match event {
            fanotify::Event::Create { p_fh, fh, name } => {
                if db::create(&tx, p_fh, fh, name)? {
                    log::warn!(
                        "Attempted to create duplicate file: `{name}`",
                    )
                }
            }
            fanotify::Event::Delete { p_fh, fh, name } => {
                match db::delete(&tx, p_fh, fh, name) {
                    Err(db::Error::NoFile { p_fh: _, fh: _, name }) => {
                        log::warn!(
                            "Attempted to delete file that doesn't exist in DB: `{}`",
                            name
                        );
                        Ok(())
                    },
                    x => x
                }?
            }
            fanotify::Event::Move { old_p_fh, new_p_fh, fh, old_name, new_name } => {
                match db::r#move(&tx, old_p_fh, new_p_fh, fh, old_name, new_name) {
                    Err(db::Error::NoFile { p_fh: _, fh: _, name }) => {
                        log::warn!(
                            "Attempted to move file that doesn't exist in DB: `{}`, \
                            Creating new file",
                            name
                        );
                        db::create(&tx, new_p_fh, fh, new_name)?;
                    },
                    _ => ()
                }
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
