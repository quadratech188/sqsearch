
use std::iter;

use indoc::indoc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("SQLite error: {0}")]
    SQLite(#[from] rusqlite::Error),
    #[error("Bad query:")]
    BadQuery(Vec<String>),
    #[error("File doesn't exist: {0}")]
    NoFile(u64, String),
    #[error("File already exists: {0}")]
    DuplicateFile(u64, u64, String)
}

pub fn map_db_err<T>(x: Result<T, rusqlite::Error>) -> Result<T, Error> {
    Ok(x?)
}

pub fn prepare_db(conn: &rusqlite::Connection) -> Result<(), Error> {

    // conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "normal")?;

    conn.execute_batch(indoc! {"
        BEGIN;

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            parent_inode INTEGER,
            inode INTEGER,
            name TEXT,
            UNIQUE(parent_inode, name)
        );

        CREATE TABLE IF NOT EXISTS suffix_array (
            parent_inode INTEGER,
            inode INTEGER,
            id INTEGER,
            suffix TEXT,
            PRIMARY KEY (inode, suffix, id)
        ) WITHOUT ROWID;

        CREATE INDEX IF NOT EXISTS forwards_idx ON suffix_array (parent_inode, suffix);
        CREATE INDEX IF NOT EXISTS suffix_idx ON suffix_array (suffix);

        COMMIT;
    "})?;

    Ok(())
}

pub fn create(tx: &rusqlite::Transaction, parent_inode: u64, inode: u64, name: &str)
-> Result<(), Error> {
    let mut file_insert = tx.prepare_cached(indoc! {"
        INSERT INTO files(parent_inode, inode, name) VALUES(?1, ?2, ?3)
        ON CONFLICT DO NOTHING
    "})?;
    // Let big inode values wrap around to negatives
    let id = file_insert.insert((parent_inode as i64, inode as i64, name))
        .map_err(|e| {
            match e {
                rusqlite::Error::StatementChangedRows(0)
                    => Error::DuplicateFile(parent_inode, inode, name.into()),
                x => x.into()
            }
        })?;

    let mut suffix_insert = tx.prepare_cached(indoc! {"
        INSERT into suffix_array VALUES(?1, ?2, ?3, ?4)
    "})?;
    for (i, _) in name.char_indices() {
        suffix_insert.execute((parent_inode as i64, inode as i64, id, &name[i..]))?;
    }
    Ok(())
}

pub fn delete(tx: &rusqlite::Transaction, parent_inode: u64, name: &str)
-> Result<(), Error> {
    let mut delete_files = tx.prepare_cached(indoc! {"
        DELETE FROM files WHERE
            parent_inode = ?1
            AND name = ?2
        RETURNING rowid
    "})?;

    let id: i64 = delete_files.query_one(
        (parent_inode as i64, name), |x| x.get(0))
        .map_err(|e| {
            match e {
                rusqlite::Error::QueryReturnedNoRows
                    => Error::NoFile(parent_inode, name.into()),
                x => x.into()
            }
        })?;

    let mut delete_suffixes = tx.prepare_cached(indoc! {"
        DELETE FROM suffix_array WHERE
            id = ?1
    "})?;

    delete_suffixes.execute((id,))?;

    Ok(())
}

fn estimate_specifity(query: &str) -> i64 {
    query.len() as i64
}

pub fn prepare_query<'a>(tx: &'a rusqlite::Transaction<'a>, segments: &[&str])
-> Result<(rusqlite::CachedStatement<'a>, Vec<String>), Error> {

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

    /*
    let header = format!(
        "SELECT s{}.id FROM fts JOIN files AS s{} ON s{}.id = fts.id",
        segments.len() - 1, join_order[0], join_order[0]);
    */

    let header = format!(
        "SELECT s{}.id FROM suffix_array AS s{}",
        segments.len() - 1, join_order[0]);

    let joins = (1..segments.len())
        .map(|x| format!("suffix_array AS s{}", join_order[x]));

    let parent_conds = (0..segments.len() - 1)
        .map(|x| format!("s{x}.inode = s{y}.parent_inode", y = x+1));

    let str_conds = (0..segments.len())
        .map(|x| {
            /*
            if x == join_order[0] {
                params.push(format!("%{}%", segments[x]));
                return format!("fts.name LIKE ?")
            }
            */
            params.push(segments[x].into());
            params.push(format!("{}\u{10FFFF}", segments[x]));

            format!("s{x}.suffix >= ? AND s{x}.suffix < ?")
        });

    // Prevent query reordering with CROSS JOIN
    let first = iter::chain([header], joins).collect::<Vec<_>>().join(" CROSS JOIN ");
    let second = iter::chain(parent_conds, str_conds).collect::<Vec<_>>().join(" AND ");

    let query = if second == "" {
        first
    }
    else {
        format!("{first} WHERE {second}")
    };


    dbg!(&query);
    dbg!(&params);

    Ok((tx.prepare_cached(&query)?, params))
}
