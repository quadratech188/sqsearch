use std::{io, sync::mpsc, thread, time};

use crate::db;


#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    DB(#[from] db::Error),
    #[error("Bad query")]
    BadQuery
}

fn print_results(
    conn: &rusqlite::Connection, rows: &mut rusqlite::Rows,
    row_length: usize, count: usize
) -> Result<usize, Error> {
    let mut cnt = 0;
    loop {
        let Some(row) = db::map_db_err(rows.next())? else {return Ok(cnt)};

        let path = db::get_path(conn, row, row_length)?;

        println!("ITEM {}", path.display());
        cnt += 1;
        if cnt == count {
            return Ok(cnt);
        }
    }
}

fn do_query(conn: &rusqlite::Connection, msg: &str) -> Result<(), Error> {
    let (count, msg) = match msg.strip_prefix("COUNT ") {
        None => (usize::MAX, msg),
        Some(x) => {
            let (count, query) = x.split_once(' ')
                .ok_or(Error::BadQuery)?;
            let count = count.parse()
                .map_err(|_| Error::BadQuery)?;
            (count, query)
        }
    };

    let segments = msg.split("/").collect::<Vec<_>>();

    let (query, params) = db::prepare_query(&segments)?;
    log::debug!("Query: {}", query);

    let mut stmt = db::map_db_err(conn.prepare_cached(&query))?;
    let mut rows = db::map_db_err(stmt.query(rusqlite::params_from_iter(params)))?;

    print_results(conn, &mut rows, segments.len(), count)?;
    Ok(())
}

pub fn query(conn: rusqlite::Connection) -> Result<(), anyhow::Error> {
    let (tx, rx) = mpsc::channel::<String>();

    let interrupt_handle = conn.get_interrupt_handle();

    let _query_thread = thread::spawn(move || {
        loop {
            let Ok(msg) = rx.recv() else {break};
            println!("BEGIN {}", msg);
            let begin = time::Instant::now();

            match do_query(&conn, &msg) {
                Ok(()) => (),
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
        let trimmed = buffer.trim_end_matches('\n').to_string();

        interrupt_handle.interrupt();

        tx.send(trimmed)?;
    }
}
