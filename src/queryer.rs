use std::{io, path, sync::mpsc, thread, time};

use crate::{GlobalArgs, db};

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Format {
    Paths
}

#[derive(clap::Args, Clone, Debug)]
pub struct QueryArgs {
    #[arg(long, default_value="paths")]
    format: Format
}

impl Format {
    fn begin(&self, query: &str) {
        println!("BEGIN {query}");
    }
    fn end(&self, query: &str) {
        println!("END {query}");
    }
    fn path(&self, path: &path::Path) {
        println!("ITEM {}", path.display())
    }
    fn error(&self, error: &db::Error) {
        println!("ERROR {error}")
    }
}

fn print_results(
    conn: &rusqlite::Connection, format: Format,
    rows: &mut rusqlite::Rows, row_length: usize, count: usize
) -> Result<usize, db::Error> {
    let mut cnt = 0;
    loop {
        let Some(row) = rows.next()? else {return Ok(cnt)};
        let path = db::get_path(conn, row, row_length)?;

        format.path(&path);

        cnt += 1;
        if cnt == count {
            return Ok(cnt);
        }
    }
}

fn do_query(conn: &rusqlite::Connection, format: Format, msg: &str) -> Result<(), db::Error> {
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

    print_results(conn, format, &mut rows, segments.len(), count)?;
    Ok(())
}

pub fn exec(globals: &GlobalArgs, args: &QueryArgs) -> anyhow::Result<()> {
    let mut conn = rusqlite::Connection::open_with_flags(
        &globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    )?;
    db::prepare_db(&mut conn)?;

    let (tx, rx) = mpsc::channel::<String>();
    let interrupt_handle = conn.get_interrupt_handle();

    let format = args.format;

    thread::spawn(move || {
        loop {
            let msg = rx.recv().unwrap();
            format.begin(&msg);
            let begin = time::Instant::now();

            match do_query(&conn, format, &msg) {
                Ok(()) => (),
                Err(db::Error::SQLite(e))
                    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
                    => (),
                Err(e) => format.error(&e)
            }

            format.end(&msg);
            log::info!("Query took {} ms", begin.elapsed().as_millis());
        }
    });

    let mut buffer = String::new();
    loop {
        buffer.clear();
        io::stdin().read_line(&mut buffer)?;

        interrupt_handle.interrupt();
        tx.send(
            buffer
                .trim_end_matches('\n')
                .to_string()
        )?;
    }
}

/*
pub fn query(:) -> Result<(), anyhow::Error> {

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
*/
