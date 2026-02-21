use std::{ffi, os::unix::ffi::{OsStrExt, OsStringExt}, path};

use indoc::indoc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Bad metadata for key {0}: {1}")]
    BadMetadata(String, rusqlite::Error),
    #[error("DB error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Bad query:")]
    BadQuery(Vec<String>),
    #[error("File doesn't exist: {name}")]
    NoFile {
        p_fh: Vec<u8>,
        fh: Vec<u8>,
        name: String
    },
    #[error("File already exists: {name}")]
    DuplicateFile{
        p_fh: Vec<u8>,
        name: String
    }
}

pub fn map_db_err<T>(x: Result<T, rusqlite::Error>) -> Result<T, Error> {
    Ok(x?)
}

pub fn prepare_db(conn: &rusqlite::Connection) -> Result<(), Error> {

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "normal")?;

    conn.execute_batch(indoc! {"
        BEGIN;

        CREATE TABLE IF NOT EXISTS metadata (
            k TEXT,
            v BLOB,
            UNIQUE(k)
        );

        CREATE TABLE IF NOT EXISTS file_handles (
            sfh INTEGER PRIMARY KEY,
            fh BLOB,
            ref_count INTEGER DEFAULT 1,
            UNIQUE(fh)
        );

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            p_sfh INTEGER,
            sfh INTEGER,
            name TEXT,
            UNIQUE(p_sfh, name)
        );

        CREATE TABLE IF NOT EXISTS suffix_array (
            p_sfh INTEGER,
            sfh INTEGER,
            id INTEGER,
            suffix TEXT,
            PRIMARY KEY (sfh, suffix, id)
        ) WITHOUT ROWID;

        CREATE INDEX IF NOT EXISTS sfh_idx ON files (sfh);
        CREATE INDEX IF NOT EXISTS id_idx ON suffix_array (id);
        CREATE INDEX IF NOT EXISTS forwards_idx ON suffix_array (p_sfh, suffix);
        CREATE INDEX IF NOT EXISTS suffix_idx ON suffix_array (suffix);

        COMMIT;
    "})?;

    Ok(())
}

fn borrow_sfh(tx: &rusqlite::Transaction, fh: &[u8]) -> Result<i64, Error> {
    let mut stmt = tx.prepare_cached(indoc! {"
        INSERT INTO file_handles(fh) VALUES(?1)
        ON CONFLICT DO UPDATE SET ref_count = ref_count + 1
        RETURNING sfh
    "})?;
    Ok(stmt.query_one((fh,), |x| x.get(0))?)
}

// FIXME: Implement return_sfh

fn create_suffixes(tx: &rusqlite::Transaction, p_sfh: i64,
    sfh: i64, id: i64, name: &str)
-> Result<(), Error> {
    let mut stmt = tx.prepare_cached(indoc! {"
        INSERT into suffix_array VALUES(?1, ?2, ?3, ?4)
    "})?;
    for (i, _) in name.char_indices() {
        stmt.execute((p_sfh, sfh, id, &name[i..]))?;
    }
    Ok(())
}

fn delete_suffixes(tx: &rusqlite::Transaction, id: i64) -> Result<(), Error> {
    let mut stmt = tx.prepare_cached(indoc! {"
        DELETE FROM suffix_array WHERE
            id = ?1
    "})?;
    stmt.execute((id,))?;
    Ok(())
}

fn reparent_suffixes(tx: &rusqlite::Transaction, p_sfh: i64, id: i64)
-> Result<(), Error> {
    let mut stmt = tx.prepare_cached(indoc! {"
        UPDATE suffix_array SET p_sfh = ?1 WHERE
            id = ?2
    "})?;
    stmt.execute((p_sfh, id))?;
    Ok(())
}

pub fn create(tx: &rusqlite::Transaction, p_fh: &[u8], fh: &[u8], name: &str)
-> Result<(), Error> {
    let p_sfh = borrow_sfh(tx, p_fh)?;
    let sfh = borrow_sfh(tx, fh)?;

    let mut create_file = tx.prepare_cached(indoc! {"
        INSERT INTO files(p_sfh, sfh, name) VALUES(?1, ?2, ?3)
        ON CONFLICT DO NOTHING
    "})?;
    let id = create_file.insert((p_sfh, sfh, name))
        .map_err(|e| {
            match e {
                rusqlite::Error::StatementChangedRows(0)
                => Error::DuplicateFile {
                    p_fh: p_fh.into(),
                    name: name.into()
                },
                x => x.into()
            }
        })?;

    create_suffixes(tx, p_sfh, sfh, id, name)
}

pub fn delete(tx: &rusqlite::Transaction, p_fh: &[u8], fh: &[u8], name: &str)
-> Result<(), Error> {
    let p_sfh = borrow_sfh(tx, p_fh)?;
    let sfh = borrow_sfh(tx, fh)?;

    let mut delete_file = tx.prepare_cached(indoc! {"
        DELETE FROM files WHERE
            p_sfh = ?1
            AND sfh = ?2
            AND name = ?3
        RETURNING id
    "})?;

    let id: i64 = delete_file.query_one((p_sfh, sfh, name), |x| x.get(0))
        .map_err(|e| {
            match e {
                rusqlite::Error::QueryReturnedNoRows
                => Error::NoFile {
                    p_fh: p_fh.into(),
                    fh: fh.into(),
                    name: name.into()
                },
                x => x.into()
            }
        })?;

    delete_suffixes(tx, id)
}

fn delete2(tx: &rusqlite::Transaction, p_sfh: i64, name: &str) -> Result<(), Error> {
    let mut delete_file = tx.prepare_cached(indoc! {"
        DELETE FROM files WHERE
            p_sfh = ?1
            AND name = ?2
        RETURNING id
    "})?;

    let id: i64 = delete_file.query_one((p_sfh, name), |x| x.get(0))?;
    delete_suffixes(tx, id)
}

fn move_impl(
    tx:&rusqlite::Transaction,
    old_p_sfh: i64, new_p_sfh: i64, sfh: i64, old_name: &str, new_name: &str
) -> Result<(), Error> {
    let mut update_file = tx.prepare_cached(indoc! {"
        UPDATE files SET p_sfh = ?1, name = ?2 WHERE
            p_sfh = ?3
            AND sfh = ?4
            AND name = ?5
        RETURNING id
    "})?;

    let id: i64 = match update_file.query_one(
        (new_p_sfh, new_name, old_p_sfh, sfh, old_name), |x| x.get(0)
    ) {
        Ok(x) => x,
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation)
        => {
            // Something like touch a b; mv a b
            // TODO: reuse suffixes of b
            delete2(tx, new_p_sfh, new_name)?;
            return move_impl(tx, old_p_sfh, new_p_sfh, sfh, old_name, new_name);
        }
        Err(x) => return Err(x.into())
    };

    if old_name == new_name {
        reparent_suffixes(tx, new_p_sfh, id)
    }
    else {
        delete_suffixes(tx, id)?;
        create_suffixes(tx, new_p_sfh, sfh, id, new_name)
    }
}

pub fn r#move(
    tx: &rusqlite::Transaction,
    old_p_fh: &[u8], new_p_fh: &[u8], fh: &[u8], old_name: &str, new_name: &str
) -> Result<(), Error> {
    match move_impl(
        tx,
        borrow_sfh(tx, old_p_fh)?,
        borrow_sfh(tx, new_p_fh)?,
        borrow_sfh(tx, fh)?,
        old_name, new_name
    ) {
        Ok(x) => Ok(x),
        Err(Error::SQLite(rusqlite::Error::QueryReturnedNoRows))
        => return Err(Error::NoFile {
            p_fh: old_p_fh.into(),
            fh: fh.into(),
            name: old_name.into()
        }),
        Err(x) => return Err(x)
    }
}

pub struct Metadata {
    prefix: path::PathBuf,
    root_sfh: i64
}

pub fn set_metadata(tx: &rusqlite::Transaction, root_path: &path::Path, root_fh: &[u8])
-> Result<(), Error> {
    let sfh = borrow_sfh(tx, root_fh)?;

    // TODO: Do something if we're overwriting
    let mut stmt = tx.prepare_cached("
        INSERT OR REPLACE INTO metadata VALUES(?1, ?2)
    ")?;

    stmt.execute(("prefix", root_path.as_os_str().as_bytes()))?;
    stmt.execute(("root_sfh", sfh))?;

    Ok(())
}

pub fn get_metadata(tx: &rusqlite::Connection) -> Result<Metadata, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT mt.v FROM metadata AS mt WHERE mt.k = ?1
    ")?;

    macro_rules! get {
        ($key: expr) => {
            stmt.query_one(($key,), |x| x.get(0))
                .map_err(|e| Error::BadMetadata($key.into(), e))
        };
    }

    let prefix: Vec<u8> = get!("prefix")?;
    let root_sfh: i64 = get!("root_sfh")?;


    Ok(Metadata {
        prefix: ffi::OsString::from_vec(prefix).into(),
        root_sfh
    })
}

pub fn get_parent_path(mt: &Metadata, tx: &rusqlite::Connection, id: i64)
-> Result<path::PathBuf, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT f.p_sfh FROM files AS f WHERE f.id = ?1
    ")?;

    let mut p_sfh: i64 = stmt.query_one((id,), |x| x.get(0))?;

    if p_sfh == mt.root_sfh {
        return Ok(mt.prefix.clone())
    }

    let mut names = vec![];

    let mut stmt = tx.prepare_cached("
        SELECT f.name, f.p_sfh FROM files AS f WHERE f.sfh = ?1
    ")?;
    loop {
        // More than one instance is OK
        let (name, new_p_sfh): (String, i64) = stmt.query_row((p_sfh,), |x| Ok((x.get(0)?, x.get(1)?)))?;

        if new_p_sfh == mt.root_sfh {break}
        names.push(name);
        p_sfh = new_p_sfh;
    }

    let mut path = mt.prefix.clone();

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

    let mut query = String::from("SELECT ");

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
        .map(|x| format!("s{}.sfh = s{}.p_sfh AND ", x, x + 1))
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
