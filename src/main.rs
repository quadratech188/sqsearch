use std::{collections::HashMap, os::unix::{ffi::OsStrExt, fs::MetadataExt}, path};

use walkdir::WalkDir;

mod db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = path::Path::new("./db");
    let mut db = rusqlite::Connection::open(path)?;

    db::prepare_db(&db)?;

    let tx = db.transaction()?;

    for entry in WalkDir::new("/usr/include") {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Error walking: {}", e);
                continue;
            }
        };

        let metadata = entry.metadata()?;
        let inode = metadata.ino();
        let parent_inode = entry
            .path()
            .parent()
            .unwrap()
            .metadata()?
            .ino();

        db::insert(&tx, parent_inode, inode, entry.file_name().try_into()?)?;
    }

    tx.commit()?;

    let tx = db.transaction()?;
    {

        let segments: Vec<_> = "a/kvm".split("/").collect();
        let params = db::prepare_params(&segments);
        let mut query = db::prepare_query(&tx, &segments)?;

        let _: Vec<_> = query.query_map(
            rusqlite::params_from_iter(params),
            |x| {
                let _ = dbg!(x.get::<usize, i64>(0));
                Ok(())
            })?.collect();
    }
    tx.commit()?;


    Ok(())
}
