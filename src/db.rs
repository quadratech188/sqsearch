use std::{ffi::{OsStr, OsString}, os::unix::ffi::OsStrExt, path};

use crate::{file_handle::FileHan, gen_suffixes_vtab};

const ROOT_ID: i64 = 1;

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum Error {
    #[error("DB error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Bad query:")]
    BadQuery,
    #[error("Failed to find path for file")]
    IncompletePath,

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


const MIGRATIONS_SLICE: &[rusqlite_migration::M<'_>] = &[
    // The files table needs to store every file/dir regardless of encoding, as we'll need to index
    // its subdirs, so we encode filenames as BLOB
    // But it doesn't make sense to run queries on invalid UTF-8, so we use TEXT for suffix_array,
    // and we just don't include entries for invalid UTF-8s at all

    rusqlite_migration::M::up(r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            file_handle BLOB,
            name BLOB,
            parent_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
            UNIQUE(parent_id, name)
        );

        CREATE INDEX fanotify_lookup_idx ON files(file_handle, name);

        CREATE TABLE suffix_array (
            suffix TEXT COLLATE NOCASE,
            id INTEGER,
            PRIMARY KEY (suffix, id)
        ) WITHOUT ROWID;

        CREATE TRIGGER add_suffixes AFTER INSERT ON files BEGIN
            INSERT INTO suffix_array
            SELECT suffix, NEW.id FROM gen_suffixes(NEW.name);
        END;

        CREATE TRIGGER delete_suffixes AFTER DELETE ON files BEGIN
            DELETE FROM suffix_array
            WHERE suffix IN (SELECT suffix FROM gen_suffixes(OLD.name))
            AND id = OLD.id;
        END;

        CREATE TRIGGER update_suffixes AFTER UPDATE ON files WHEN OLD.name != NEW.name BEGIN
            DELETE FROM suffix_array
            WHERE suffix IN (SELECT suffix FROM gen_suffixes(OLD.name))
            AND id = OLD.id;

            INSERT INTO suffix_array
            SELECT suffix, NEW.id FROM gen_suffixes(NEW.name);
        END;
    "#)
];

const MIGRATIONS: rusqlite_migration::Migrations<'_>
    = rusqlite_migration::Migrations::from_slice(MIGRATIONS_SLICE);

pub fn prepare_db(conn: &mut rusqlite::Connection) -> anyhow::Result<()> {

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "normal")?;
    conn.pragma_update(None, "busy_timeout", -2000)?;
    conn.pragma_update(None, "cache_size", -64 * 1024)?;
    conn.pragma_update(None, "mmap_size", 1024 * 1024 * 1024)?;

    conn.pragma_update(None, "foreign_keys", true)?;

    conn.create_module("gen_suffixes", &gen_suffixes_vtab::MODULE, None)?;

    MIGRATIONS.to_latest(conn)?;

    Ok(())
}

pub fn ensure_root(tx: &rusqlite::Transaction, fh: &FileHan) -> Result<(), Error> {
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

pub fn get_single_id(tx: &rusqlite::Transaction, fh: &FileHan) -> Result<i64, Error> {
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

pub fn get_id(tx: &rusqlite::Transaction, parent_id: i64, fh: &FileHan, name: &OsStr)
-> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE file_handle = ?1 AND name = ?2 AND parent_id = ?3
    ")?;

    transform_query_one_err(stmt.query_one((fh, name.as_bytes(), parent_id), |x| x.get(0)))
}

fn get_name(tx: &rusqlite::Transaction, id: i64) -> Result<OsString, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT name FROM files WHERE id = ?1;
    ")?;

    Ok(unsafe {OsString::from_encoded_bytes_unchecked(
        stmt.query_one((id,), |x| x.get(0))?
    )})
}

pub fn create(tx: &rusqlite::Transaction, parent_id: i64, fh: &FileHan, name: &OsStr)
-> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        INSERT INTO files (file_handle, parent_id, name) VALUES(?1, ?2, ?3)
        ON CONFLICT DO NOTHING
    ")?;

    match stmt.insert((fh, parent_id, name.as_bytes())) {
        Ok(x) => Ok(x),
        Err(rusqlite::Error::StatementChangedRows(0)) =>
            Err(Error::DuplicateFile),
        Err(e) => Err(e.into())
    }
}

pub fn delete(tx: &rusqlite::Transaction, id: i64)
-> Result<(), Error> {
    let mut stmt = tx.prepare_cached("
        DELETE FROM files WHERE id = ?1
    ")?;

    let rows_changed = stmt.execute((id,))?;
    
    match rows_changed {
        0 => Err(Error::NoFile),
        1 => Ok(()),
        _ => Err(Error::DuplicateFile)
    }
}

pub fn r#move(
    tx:&rusqlite::Transaction, id: i64,
    new_parent_id: i64, new_name: &OsStr
) -> Result<(), Error> {
    let mut stmt = tx.prepare_cached("
        UPDATE files SET parent_id = ?1, name = ?2 WHERE id = ?3
    ")?;

    match stmt.execute((new_parent_id, new_name.as_bytes(), id)) {
        Ok(x) => Ok(x),
        Err(e) if e.sqlite_error_code() ==
            Some(rusqlite::ErrorCode::ConstraintViolation) =>
            Err(Error::DuplicateFile),
        Err(x) => Err(x.into())
    }?;

    Ok(())
}

pub fn get_path(tx: &rusqlite::Connection, row: &rusqlite::Row, segment_cnt: usize)
-> Result<path::PathBuf, Error> {
    // row[0]: s0.id
    // row[1 ... segment_cnt]: s0.name ... s{segment_cnt - 1}.name

    let mut stmt = tx.prepare_cached("
        SELECT f.name, parent_id FROM files AS f WHERE f.id = ?1
    ")?;

    let mut names = vec![];
    let mut ptr = row.get(0)?;
    let mut cnt = 0;
    
    loop {
        let (name, parent_id): (OsString, i64) = match stmt.query_one((ptr,), |x| {
                Ok((
                    unsafe {OsString::from_encoded_bytes_unchecked(x.get(0)?)},
                    x.get(1)?
                ))
            }) {
            Ok(x) => Ok(x),
            Err(rusqlite::Error::QueryReturnedNoRows)
            => Err(Error::IncompletePath),
            Err(e) => Err(e.into())
        }?;

        names.push(name);

        if parent_id == ROOT_ID {break}
        ptr = parent_id;

        cnt += 1;
        if cnt > 1000 {
            return Err(Error::IncompletePath)
        }
    }

    let mut path = path::PathBuf::new();

    for p in names.iter().rev() {
        path.push(p);
    }

    // s0.name is already inserted
    for i in 1..segment_cnt {
        let s: Vec<u8> = row.get(i + 1)?;
        path.push(unsafe {OsStr::from_encoded_bytes_unchecked(&s)});
    }

    Ok(path)
}

fn estimate_specifity(segments: &[&str]) -> Vec<i64> {
    segments.iter().map(|x| x.len() as i64).collect()
}

pub fn prepare_query(segments: &[&str]) -> Result<(String, Vec<String>), Error> {
    let scores = estimate_specifity(segments);

    let Some(best_index) = (0..scores.len()).max_by_key(|&x| scores[x]) else {
        return Err(Error::BadQuery)
    };

    let mut joins = vec![];
    let mut params = vec![];
    let mut conditions = vec![];

    joins.push("suffix_array".to_string());
    params.push(segments[best_index].to_string());
    params.push(format!("{}\u{10FFFF}", segments[best_index]));
    conditions.push(format!("suffix_array.suffix >= ? AND suffix_array.suffix < ?"));

    joins.push(format!("files AS s{best_index}"));
    conditions.push(format!("s{best_index}.id = suffix_array.id"));

    let mut l = best_index;
    let mut r = best_index;

    while 0 < l || r < scores.len() - 1 {
        if 0 == l || (r < scores.len() - 1 && scores[l - 1] < scores[r + 1]) {
            r += 1;

            joins.push(format!("files AS s{r}"));
            conditions.push(format!("s{}.parent_id = s{}.id", r, r - 1));

            if segments[r] == "" {continue}

            params.push(format!("%{}%", segments[r].to_string()));
            conditions.push(format!("LOWER(CAST(s{r}.name AS TEXT)) LIKE LOWER(?)"));
        }
        else {
            l -= 1;

            joins.push(format!("files AS s{l}"));
            conditions.push(format!("s{}.id = s{}.parent_id", l, l + 1));

            if segments[l] == "" {continue}

            params.push(format!("%{}%", segments[l].to_string()));
            conditions.push(format!("LOWER(CAST(s{l}.name AS TEXT)) LIKE LOWER(?)"));
        }
    }

    let mut query = String::from("SELECT DISTINCT s0.id, ");

    query += &(0..segments.len())
        .map(|x| format!("s{x}.name"))
        .collect::<Vec<_>>()
        .join(", ");

    query += " FROM ";
    query += &joins.join(" CROSS JOIN ");
    query += " WHERE ";
    query += &conditions.join(" AND ");

    Ok((query, params))
}

#[cfg(test)]
mod tests {
    use std::ops::Deref;

use crate::{file_handle::FileHandle, util};

use super::*;

    fn memory_db() -> anyhow::Result<rusqlite::Connection> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        prepare_db(&mut conn)?;
        Ok(conn)
    }

    fn make_fh(id: i64) -> FileHandle {
        const LEN: usize = 24;
        const TOTAL_LEN: usize = size_of::<util::file_handle>() + LEN;

        let mut buf = [0; TOTAL_LEN];

        buf[..8].copy_from_slice(&LEN.to_ne_bytes());
        buf[8..16].copy_from_slice(&id.to_ne_bytes());

        FileHan::read_from_buf(&buf).unwrap().to_owned()
    }

    fn count_rows(tx: &rusqlite::Transaction, name: &str) -> anyhow::Result<usize> {
        let mut stmt = tx.prepare_cached(&format!("
            SELECT * FROM {name}
        "))?;
        Ok(stmt.query_map((), |_| Ok(()))?.count())
    }

    #[test]
    fn make_db() -> anyhow::Result<()> {
        memory_db()?;
        Ok(())
    }

    #[test]
    fn triggers_test() -> anyhow::Result<()> {
        let mut conn = memory_db()?;
        let tx = conn.transaction()?;

        let mut create = tx.prepare_cached("
            INSERT INTO files (file_handle, name, parent_id) VALUES(?1, ?2, ?3)
        ")?;

        let mut delete = tx.prepare_cached("
            DELETE FROM files WHERE id = ?1
        ")?;

        let mut rename = tx.prepare_cached("
            UPDATE files SET parent_id = ?1, name = ?2 WHERE id = ?3
        ")?;

        // Root: Set parent_id = NULL
        let root = create.insert((make_fh(1).deref(), "".as_bytes(), None::<i64>))?;

        let id1 = create.insert((make_fh(1).deref(), "test1".as_bytes(), root))?;
        assert_eq!(count_rows(&tx, "suffix_array")?, 5);

        let id2 = create.insert((make_fh(2).deref(), "test2".as_bytes(), root))?;
        assert_eq!(count_rows(&tx, "suffix_array")?, 10);

        delete.execute((id1,))?;
        assert_eq!(count_rows(&tx, "suffix_array")?, 5);

        rename.execute((root, "long-name".as_bytes(), id2))?;
        assert_eq!(count_rows(&tx, "suffix_array")?, 9);

        Ok(())
    }

    #[test]
    fn ripple_delete() -> anyhow::Result<()> {
        let mut conn = memory_db()?;
        let tx = conn.transaction()?;

        let mut create = tx.prepare_cached("
            INSERT INTO files (file_handle, name, parent_id) VALUES(?1, ?2, ?3)
        ")?;

        let mut delete = tx.prepare_cached("
            DELETE FROM files WHERE id = ?1
        ")?;

        let root = create.insert((make_fh(1).deref(), "".as_bytes(), None::<i64>))?;
        let id1 = create.insert((make_fh(1).deref(), "test1".as_bytes(), root))?;
        create.insert((make_fh(2).deref(), "test2".as_bytes(), id1))?;

        assert_eq!(count_rows(&tx, "files")?, 3);

        delete.execute((root,))?;
        assert_eq!(count_rows(&tx, "files")?, 0);

        Ok(())
    }
}
