use std::{io, sync::mpsc, thread, time};

use crate::db;


#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IO(#[from] io::Error),
    #[error("OS error: {0}")]
    OS(#[from] errno::Errno),
    // Overlaps with DB error, oh well
    #[error("DB error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error(transparent)]
    DB(#[from] db::Error),
    #[error("Messaging")]
    Messaging
}

fn do_query(mt: &db::Metadata, conn: &rusqlite::Connection, msg: &str) -> Result<(), Error> {
    let segments = msg.split("/").collect::<Vec<_>>();

    let (query, params) = db::prepare_query(&segments)?;
    log::debug!("Query: {}", query);

    let mut stmt = conn.prepare_cached(&query)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;

    loop {
        let Some(row) = rows.next()? else {return Ok(())};

        let ids = (0..segments.len())
            .map(|x| row.get(x))
            .collect::<Result<Vec<i64>, rusqlite::Error>>()?;

        dbg!(&ids);

        let mut stmt = conn.prepare_cached("
            SELECT f.name FROM files AS f WHERE f.id = ?1
        ")?;

        let names = ids.iter().map(|x| stmt.query_one((x,), |x| x.get(0)))
            .collect::<Result<Vec<String>, rusqlite::Error>>()?;

        let mut path = match db::get_parent_path(mt, conn, ids[0]) {
            Ok(x) => x,
            Err(db::Error::IncompletePath(id)) => {
                log::warn!("Incomplete path for {id}");
                continue
            }
            Err(e) => return Err(e.into())
        };

        for name in names {
            path.push(name);
        }
        println!("ITEM {}", path.display());
    }
}

pub fn query(conn: rusqlite::Connection) -> Result<(), Error> {
    let (tx, rx) = mpsc::channel::<String>();

    let mt = db::get_metadata(&conn)?;
    let interrupt_handle = conn.get_interrupt_handle();

    let _query_thread = thread::spawn(move || {
        loop {
            let Ok(msg) = rx.recv() else {break};
            println!("BEGIN {}", msg);
            let begin = time::Instant::now();

            match do_query(&mt, &conn, &msg) {
                Ok(()) => (),

                // Having two cases of rusqlite errors was a big mistake

                Err(Error::SQLite(e)) if e.sqlite_error_code()
                    == Some(rusqlite::ErrorCode::OperationInterrupted)
                    => (),
                Err(Error::DB(db::Error::SQLite(e))) if e.sqlite_error_code()
                    == Some(rusqlite::ErrorCode::OperationInterrupted)
                    => (),

                Err(e) => println!("ERROR {}", e)
            }

            println!("END {}", msg);
            log::info!("Query took {} ms", begin.elapsed().as_millis());
        }
    });

    let stdin = io::stdin();

    loop {
        let mut buffer = String::new();
        stdin.read_line(&mut buffer)?;
        let trimmed = buffer.trim_end().to_string();

        interrupt_handle.interrupt();

        tx.send(trimmed).map_err(|_| Error::Messaging)?;
    }
}
