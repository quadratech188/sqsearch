use std::collections::{HashMap, hash_map::Entry};

use indoc::indoc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("SQLite error: {0}")]
    SQLite(#[from] rusqlite::Error)
}


pub fn prepare_db(conn: &rusqlite::Connection) -> Result<(), Error> {

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "normal")?;

    conn.execute_batch(indoc! {"
        BEGIN;

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            inode INTEGER,
            parent_inode INTEGER,
            name TEXT,
            UNIQUE(inode, parent_inode, name)
        );

        CREATE TABLE IF NOT EXISTS suffix_array (
            parent_inode INTEGER,
            inode INTEGER,
            id INTEGER,
            suffix TEXT,
            PRIMARY KEY (parent_inode, suffix, id)
        ) WITHOUT ROWID;

        CREATE INDEX IF NOT EXISTS suffix_idx ON suffix_array (suffix);

        COMMIT;
    "})?;

    Ok(())
}

pub fn insert(tx: &rusqlite::Transaction, parent_inode: u64, inode: u64, name: &str) -> Result<(), Error> {
    let mut file_insert = tx.prepare_cached(indoc! {"
        INSERT INTO files(inode, parent_inode, name) VALUES(?1, ?2, ?3)
        ON CONFLICT DO NOTHING
    "})?;
    // Let big inode values wrap around to negatives
    let id = match file_insert.insert((inode as i64, parent_inode as i64, name)) {
        Ok(x) => x,
        // Already exists - return early
        Err(rusqlite::Error::StatementChangedRows(0)) => return Ok(()),
        Err(x) => return Err(x.into())
    };

    let mut suffix_insert = tx.prepare_cached(indoc! {"
        INSERT into suffix_array VALUES(?1, ?2, ?3, ?4)
    "})?;
    for (i, _) in name.char_indices() { suffix_insert.execute((parent_inode as i64, inode as i64, id, &name[i..]))?;
    }
    Ok(())
}

pub fn prepare_query<'a>(tx: &'a rusqlite::Transaction<'a>, segments: &[&str]) -> Result<rusqlite::CachedStatement<'a>, Error> {
    let length = segments.len();
    let mut query = format!(
        "SELECT s{}.id FROM suffix_array AS s0 ",
        length - 1);
    for i in 1..length {
        query += &format!(
            "JOIN suffix_array AS s{} ON s{}.parent_inode = s{}.inode ",
            i, i, i - 1);
    }
    query += "WHERE s0.suffix >= ? AND s0.suffix < ? ";
    for i in 1..length {
        query += &format!(
            "AND s{}.suffix >= ? AND s{}.suffix < ? ",
            i, i)
    }
    Ok(tx.prepare_cached(&query)?)
}

// Return the data, not the iterator
pub fn prepare_params(segments: &[&str]) -> Vec<String> {
    segments.iter()
        .flat_map(|&x| vec![x.to_string(), format!("{}\u{10FFFF}", x)])
        .collect()
}
