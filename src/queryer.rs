use std::{io, sync::mpsc, thread, time};

use crate::db;

fn print_results(
    conn: &rusqlite::Connection, rows: &mut rusqlite::Rows,
    row_length: usize, count: usize
) -> Result<usize, db::Error> {
    let mut cnt = 0;
    loop {
        let Some(row) = rows.next()? else {return Ok(cnt)};

        let path = db::get_path(conn, row, row_length)?;

        println!("ITEM {}", path.display());
        cnt += 1;
        if cnt == count {
            return Ok(cnt);
        }
    }
}

fn do_query(conn: &rusqlite::Connection, msg: &str) -> Result<(), db::Error> {
    let (count, msg) = match msg.strip_prefix("COUNT ") {
        None => (usize::MAX, msg),
        Some(x) => {
            let Some((count, query)) = x.split_once(' ') else {return Ok(())};
            let Ok(count) = count.parse() else {return Ok(())};
            (count, query)
        }
    };

    let segments = msg.split("/").collect::<Vec<_>>();

    let Some((query, params)) = db::prepare_query(&segments) else {return Ok(())};
    log::debug!("Query: {}", query);

    let mut stmt = conn.prepare_cached(&query)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;

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
                Err(db::Error::SQLite(e)) if e.sqlite_error_code()
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
