use std::path;

use anyhow::Context;

use crate::{db, util};

fn index_path(tx: &mut rusqlite::Transaction, path: &path::Path) -> Result<(), anyhow::Error> {
    let Some(parent) = path.parent() else {
        return Err(anyhow::Error::msg(
            format!("Failed to get parent of path `{}`", path.display())
        ));
    };

    let Some(filename) = path.file_name() else {
        return Err(anyhow::Error::msg(
            format!("Failed to get filename of path `{}`", path.display())
        ));
    };

    let (_, fh) = util::get_fh(path)
        .with_context(|| format!("Failed to get file handle of `{}`", path.display()))?;

    let (_, p_fh) = util::get_fh(parent)
        .with_context(|| format!("Failed to get file handle of `{}`", parent.display()))?;

    let parent_id = db::get_single_id(&tx, &p_fh)
        .with_context(|| format!("Failed to get database ID of `{}`", parent.display()))?;

    match db::create(&tx, parent_id, &fh, filename) {
        Err(db::Error::DuplicateFile) => {
            // ignore
        }
        Err(e) => {
            return Err(e.into())
        },
        _ => ()
    }

    Ok(())
}

pub fn index(conn: &mut rusqlite::Connection, path: &path::Path) -> Result<(), anyhow::Error> {
    let mut tx = conn.transaction()?;

    db::ensure_root(&tx, &util::get_fh(path)?.1)?;

    for (i, entry) in walkdir::WalkDir::new(path).into_iter().enumerate() {
        let Ok(entry) = entry else {continue};
        if entry.path() == path {continue};

        let path = entry.path();

        if let Err(e) = index_path(&mut tx, path) {
            log::warn!("{}", e);
        }

        if i % 1000 == 0 {
            log::info!("{} {}", i, path.display());
        }
        if i % 10000 == 0 {
            db::map_db_err(tx.commit())?;
            tx = db::map_db_err(conn.transaction())?;
        }
    }

    tx.commit()?;

    Ok(())
}
