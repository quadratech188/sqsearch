use std::{ffi::{OsStr, OsString}, os::unix::ffi::OsStrExt, path};

const ROOT_ID: i64 = 1;

use indoc::indoc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("DB error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Bad query:")]
    BadQuery(Vec<String>),
    #[error("One or more elements of path for file {0} doesn't exist in DB")]
    IncompletePath(i64),

    #[error("File doesn't exist")]
    NoFile,
    #[error("Two or more files satisfying the criteria exist")]
    DuplicateFile,

    #[error("Provided file handle is not the root")]
    BadRoot
}

pub fn map_db_err<T>(x: Result<T, rusqlite::Error>) -> Result<T, Error> {
    Ok(x?)
}

pub fn prepare_db(conn: &rusqlite::Connection) -> Result<(), Error> {

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "normal")?;

    // The files table needs to store every file/dir regardless of encoding, as we'll need to index
    // its subdirs, so we encode filenames as BLOB
    // But it doesn't make sense to run queries on invalid UTF-8, so we use TEXT for suffix_array,
    // and we just don't include entries for invalid UTF-8s at all

    conn.execute_batch(indoc! {"
        BEGIN;

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            file_handle BLOB,
            name BLOB,
            parent_id INTEGER,
            UNIQUE(parent_id, name)
        );

        CREATE INDEX IF NOT EXISTS fanotify_lookup_idx ON files (file_handle, name);

        CREATE TABLE IF NOT EXISTS suffix_array (
            parent_id INTEGER,
            id INTEGER,
            suffix TEXT COLLATE NOCASE,
            PRIMARY KEY (parent_id, suffix, id)
        ) WITHOUT ROWID;

        CREATE INDEX IF NOT EXISTS backwards_idx ON suffix_array (id, suffix);
        CREATE INDEX IF NOT EXISTS suffix_idx ON suffix_array (suffix);

        COMMIT;
    "})?;

    Ok(())
}

pub fn ensure_root(tx: &rusqlite::Transaction, fh: &[u8]) -> Result<(), Error> {
    match get_single_id(tx, fh) {
        Ok(ROOT_ID) => {
            Ok(())
        }
        Ok(_) => {
            Err(Error::BadRoot)
        }
        Err(Error::NoFile) => {
            create(tx, ROOT_ID, fh, OsStr::new(""))?;
            Ok(())
        }
        Err(e) => Err(e)
    }
}

fn transform_query_one_err<T>(x: Result<T, rusqlite::Error>) -> Result<T, Error> {
    match x {
        Ok(x) => Ok(x),
        Err(rusqlite::Error::QueryReturnedMoreThanOneRow) =>
            Err(Error::DuplicateFile),
        Err(rusqlite::Error::QueryReturnedNoRows) =>
            Err(Error::NoFile),
        Err(x) => Err(Error::SQLite(x))
    }
}

pub fn get_single_id(tx: &rusqlite::Transaction, fh: &[u8]) -> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE file_handle = ?1
    ")?;

    transform_query_one_err(stmt.query_one((fh,), |x| x.get(0)))
}

pub fn get_rough_id(tx: &rusqlite::Transaction, parent_id: i64, name: &OsStr)
-> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE name = ?1 AND parent_id = ?2
    ")?;

    transform_query_one_err(stmt.query_one((name.as_bytes(), parent_id), |x| x.get(0)))
}

pub fn get_id(tx: &rusqlite::Transaction, parent_id: i64, fh: &[u8], name: &OsStr)
-> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE file_handle = ?1 AND name = ?2 AND parent_id = ?3
    ")?;

    transform_query_one_err(stmt.query_one((fh, name.as_bytes(), parent_id), |x| x.get(0)))
}

fn create_suffixes(tx: &rusqlite::Transaction, parent_id: i64, id: i64, name: &OsStr)
-> Result<(), Error> {
    // Return early for invalid UTF-8
    let Some(name) = name.to_str() else {return Ok(())};

    let mut stmt = tx.prepare_cached("
        INSERT INTO suffix_array (parent_id, id, suffix) VALUES(?1, ?2, ?3)
    ")?;
    for (i, _) in name.char_indices() {
        stmt.execute((parent_id, id, &name[i..]))?;
    }
    Ok(())
}

pub fn create(tx: &rusqlite::Transaction, parent_id: i64, fh: &[u8], name: &OsStr)
-> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        INSERT INTO files (file_handle, parent_id, name) VALUES(?1, ?2, ?3)
        ON CONFLICT DO NOTHING
    ")?;

    let id = match stmt.insert((fh, parent_id, name.as_bytes())) {
        Ok(x) => Ok(x),
        Err(rusqlite::Error::StatementChangedRows(0)) =>
            Err(Error::DuplicateFile),
        Err(e) => Err(e.into())
    }?;

    create_suffixes(tx, parent_id, id, name)?;
    Ok(id)
}

fn delete_suffixes(tx: &rusqlite::Transaction, id: i64) -> Result<(), Error> {
    let mut stmt = tx.prepare_cached("
        DELETE FROM suffix_array WHERE id = ?1
    ")?;
    stmt.execute((id,))?;
    Ok(())
}

pub fn delete(tx: &rusqlite::Transaction, id: i64)
-> Result<(), Error> {
    // We have to return something for query_one to work
    let mut stmt = tx.prepare_cached("
        DELETE FROM files WHERE id = ?1 RETURNING id
    ")?;

    transform_query_one_err(stmt.query_one((id,), |_| {Ok(())}))?;

    delete_suffixes(tx, id)?;
    Ok(())
}

fn reparent_suffixes(tx: &rusqlite::Transaction, parent_id: i64, id: i64)
-> Result<(), Error> {
    let mut stmt = tx.prepare_cached("
        UPDATE suffix_array SET parent_id = ?1 WHERE id = ?2
    ")?;
    stmt.execute((parent_id, id))?;
    Ok(())
}

pub fn r#move(
    tx:&rusqlite::Transaction, id: i64,
    new_parent_id: i64, new_name: &OsStr
) -> Result<(), Error> {
    let mut stmt = tx.prepare_cached(indoc! {"
        UPDATE files SET parent_id = ?1, name = ?2 WHERE id = ?3
    "})?;

    match stmt.execute((new_parent_id, new_name.as_bytes(), id)) {
        Ok(x) => Ok(x),
        Err(e) if e.sqlite_error_code() ==
            Some(rusqlite::ErrorCode::ConstraintViolation) =>
            Err(Error::DuplicateFile),
        Err(x) => Err(x.into())
    }?;

    // TODO: Reuse suffixes if old name == new name

    delete_suffixes(tx, id)?;
    create_suffixes(tx, new_parent_id, id, new_name)
}

pub fn get_path(tx: &rusqlite::Connection, id: i64)
-> Result<path::PathBuf, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT f.name, parent_id FROM files AS f WHERE f.id = ?1
    ")?;

    let mut names = vec![];
    let mut ptr = id;
    let mut cnt = 0;
    
    loop {
        // More than one instance is OK
        let (name, parent_id): (OsString, i64) = match stmt.query_row((ptr,), |x| {
                Ok((
                    unsafe {OsString::from_encoded_bytes_unchecked(x.get(0)?)},
                    x.get(1)?
                ))
            }) {
            Ok(x) => Ok(x),
            Err(rusqlite::Error::QueryReturnedNoRows)
            => Err(Error::IncompletePath(id)),
            Err(e) => Err(e.into())
        }?;

        names.push(name);

        if parent_id == ROOT_ID {break}
        ptr = parent_id;

        cnt += 1;
        if cnt > 1000 {
            return Err(Error::IncompletePath(id))
        }
    }

    let mut path = path::PathBuf::new();

    for p in names.iter().rev() {
        path.push(p);
    }

    Ok(path)
}

fn estimate_specifity(query: &str) -> i64 {
    query.len() as i64
}

pub fn prepare_query<'a>(segments: &[&str]) -> Result<(String, Vec<String>), Error> {

    let score = |x| estimate_specifity(segments[x]);

    let Some(best_segment) = (0..segments.len())
        .max_by_key(|&x| score(x)) else {
        return Err(Error::BadQuery(segments.iter().map(|&x| x.into()).collect()))
    };

    // TODO: Allow multiple best segments if they're specific enough
    
    let mut l: i64 = best_segment as i64 - 1;
    let mut r: i64 = best_segment as i64 + 1;
    let length = segments.len() as i64;

    let mut join_order = vec![best_segment];

    // l underflows to 2^64 - 1 > segments.len()
    while 0 <= l || r < length {
        if length <= r ||
            (0 <= l && r < length && score(l as usize) > score(r as usize)) {
            join_order.push(l as usize);
            l -= 1;
        }
        else {
            join_order.push(r as usize);
            r += 1;
        }
    }

    let mut params: Vec<String> = vec![];

    let len = segments.len();

    let mut query = String::from("SELECT DISTINCT ");

    query += &(0..len)
        .map(|x| format!("s{}.id AS s{}", x, x))
        .collect::<Vec<_>>()
        .join(", ");

    query += " FROM ";

    query += &(0..len)
        .map(|x| format!("suffix_array AS s{}", join_order[x]))
        .collect::<Vec<_>>()
        .join(" CROSS JOIN ");

    query += " WHERE ";

    query += &(0..len - 1)
        .map(|x| format!("s{}.id = s{}.parent_id AND ", x, x + 1))
        .collect::<Vec<_>>()
        .concat();

    query += &(0..len)
        .map(|x| {
                params.push(segments[x].into());
                params.push(format!("{}\u{10FFFF}", segments[x]));
                format!("s{x}.suffix >= ? AND s{x}.suffix < ?")
            })
        .collect::<Vec<_>>()
        .join(" AND ");

    Ok((query, params))
}
