use std::{ffi::{OsStr, OsString}, os::unix::ffi::OsStrExt, path};

use crate::{file_handle::FileHan, gen_suffixes_vtab};

const ROOT_ID: i64 = 1;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Misc DB errror: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    #[error("Multiple files matching the criteria already exist")]
    ManyFiles,
    #[error("No files matching the criteria exist")]
    NoFile,
    #[error("A file with the same parent and name already exists")]
    NameTaken,

    #[error("Provided file is not root of file tree")]
    BadRoot
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

pub fn prepare_db(conn: &mut rusqlite::Connection) -> Result<(), Error> {
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
    match get_dir_id(tx, fh) {
        Ok(ROOT_ID) => Ok(()),
        Ok(_) => Err(Error::BadRoot),
        Err(Error::NoFile) => {
            let id = create(tx, fh, OsStr::new(""), ROOT_ID)?;
            if id == ROOT_ID {Ok(())} else {Err(Error::BadRoot)}
        }
        Err(e) => Err(e)
    }
}

fn transform_query_one_err<T>(x: Result<T, rusqlite::Error>) -> Result<T, Error> {
    match x {
        Ok(x) => Ok(x),
        Err(rusqlite::Error::QueryReturnedMoreThanOneRow) => Err(Error::ManyFiles),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(Error::NoFile),
        Err(e) => Err(Error::SQLite(e))
    }
}

// Only files can share file handles; directory file handles are unique
pub fn get_dir_id(tx: &rusqlite::Transaction, fh: &FileHan) -> Result<i64, Error> {
    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE file_handle = ?1
    ")?;

    transform_query_one_err(
        stmt.query_one((fh,), |row| row.get(0))
    )
}

pub fn get_dirent_id(tx: &rusqlite::Transaction, name: &OsStr, p_id: i64)
-> Result<i64, Error> {

    let mut stmt = tx.prepare_cached("
        SELECT id FROM files WHERE name = ?1 AND parent_id = ?2
    ")?;

    transform_query_one_err(
        stmt.query_one((name.as_bytes(), p_id), |row| row.get(0))
    )
}

pub fn create(tx: &rusqlite::Transaction, fh: &FileHan, name: &OsStr, p_id: i64)
-> Result<i64, Error> {

    let mut stmt = tx.prepare_cached("
        INSERT INTO files (file_handle, name, parent_id)
        VALUES(?1, ?2, ?3)
    ")?;

    match stmt.insert((fh, name.as_bytes(), p_id)) {
        Ok(id) => Ok(id),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation

            => Err(Error::NameTaken),
        Err(e) => Err(Error::SQLite(e))
    }
}

pub fn delete(tx: &rusqlite::Transaction, fh: &FileHan, name: &OsStr, p_id: i64)
-> Result<(), Error> {

    let mut stmt = tx.prepare_cached("
        DELETE FROM files
        WHERE file_handle = ?1 AND name = ?2 AND parent_id = ?3
    ")?;

    match stmt.execute((fh, name.as_bytes(), p_id))? {
        0 => Err(Error::NoFile),
        1 => Ok(()),
        _ => Err(Error::ManyFiles)
    }
}

pub fn delete_with_id(tx: &rusqlite::Transaction, id: i64) -> Result<(), Error> {
    let mut stmt = tx.prepare_cached("
        DELETE FROM files
        WHERE id = ?1
    ")?;

    match stmt.execute((id,))? {
        0 => Err(Error::NoFile),
        1 => Ok(()),
        _ => Err(Error::ManyFiles)
    }
}

pub fn update(
    tx: &rusqlite::Transaction, fh: &FileHan,
    old_name: &OsStr, old_p_id: i64,
    new_name: &OsStr, new_p_id: i64
) -> Result<(), Error> {

    let mut stmt = tx.prepare_cached("
        UPDATE files SET name = ?1, parent_id = ?2
        Where file_handle = ?3 AND name = ?4 AND parent_id = ?5
    ")?;

    match stmt.execute((new_name.as_bytes(), new_p_id, fh, old_name.as_bytes(), old_p_id)) {
        Ok(0) => Err(Error::NoFile),
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation

            => Err(Error::NameTaken),
        Err(e) => Err(Error::SQLite(e))
    }
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
    
    loop {
        let (name, parent_id): (OsString, i64) = stmt.query_one((ptr,), |x| {
            Ok((
                unsafe {OsString::from_encoded_bytes_unchecked(x.get(0)?)},
                x.get(1)?
            ))
        })?;
        names.push(name);

        if parent_id == ROOT_ID {break}
        ptr = parent_id;
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

pub fn prepare_query(segments: &[&str]) -> Option<(String, Vec<String>)> {
    let scores = estimate_specifity(segments);

    let Some(best_index) = (0..scores.len()).max_by_key(|&x| scores[x]) else {return None};

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

    Some((query, params))
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

    fn assert_query_plan<Params>(
        conn: &rusqlite::Connection,
        query: &str, params: Params, expect: Vec<&str>
    ) -> anyhow::Result<()>
    where Params: rusqlite::Params {
        let mut stmt = conn.prepare_cached(&format!(
            "EXPLAIN QUERY PLAN {query}"
        ))?;

        let result: Result<Vec<String>, _> = stmt.query_map(params, |row| row.get(3))?.collect();
        let result = result?;

        assert_eq!(result, expect);
        Ok(())
    }

    #[test]
    fn query_plans() -> anyhow::Result<()> {
        let conn = memory_db()?;

        assert_query_plan(
            &conn,
            "
                INSERT INTO suffix_array
                SELECT suffix, ?1 FROM gen_suffixes(?2);
            ",
            (1, "a".as_bytes()),
            vec![
                "SCAN gen_suffixes VIRTUAL TABLE INDEX 0:"
            ]
        )?;
        assert_query_plan(
            &conn,
            "
                DELETE FROM suffix_array
                WHERE suffix IN (SELECT suffix FROM gen_suffixes(?1))
                AND id = ?2;
            ",
            (1, "a".as_bytes()),
            vec![
                "SEARCH suffix_array USING PRIMARY KEY (suffix=? AND id=?)",
                "LIST SUBQUERY 1",
                "SCAN gen_suffixes VIRTUAL TABLE INDEX 0:",
                "CREATE BLOOM FILTER"
            ]
        )?;

        Ok(())
    }

    #[test]
    fn check_query() -> anyhow::Result<()> {
        let (query, params) = prepare_query(&["a", "long", "b"]).unwrap();
        assert_eq!(
            query,
            [
                "SELECT DISTINCT s0.id, s0.name, s1.name, s2.name FROM suffix_array",
                "CROSS JOIN files AS s1",
                "CROSS JOIN files AS s0",
                "CROSS JOIN files AS s2 WHERE",
                "suffix_array.suffix >= ? AND suffix_array.suffix < ?",
                "AND s1.id = suffix_array.id",
                "AND s0.id = s1.parent_id",
                "AND LOWER(CAST(s0.name AS TEXT)) LIKE LOWER(?)",
                "AND s2.parent_id = s1.id",
                "AND LOWER(CAST(s2.name AS TEXT)) LIKE LOWER(?)"
            ].join(" ")
        );
        assert_eq!(params, vec!["long", "long\u{10ffff}", "%a%", "%b%"]);

        assert_query_plan(
            &memory_db()?,
            &query,
            rusqlite::params_from_iter(params),
            vec![
                "SEARCH suffix_array USING PRIMARY KEY (suffix>? AND suffix<?)",
                "SEARCH s1 USING INTEGER PRIMARY KEY (rowid=?)",
                "SEARCH s0 USING INTEGER PRIMARY KEY (rowid=?)",
                "SEARCH s2 USING COVERING INDEX sqlite_autoindex_files_1 (parent_id=?)",
                "USE TEMP B-TREE FOR DISTINCT"
            ]
        )?;

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
