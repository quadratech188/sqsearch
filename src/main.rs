use std::{env, ops, os::unix::fs::MetadataExt, path};

use walkdir::WalkDir;

mod db;
mod fanotify;

fn main() -> Result<(), Box<dyn std::error::Error>> {

    let path = path::Path::new("./db.sqlite3");
    let mut db = rusqlite::Connection::open(path)?;

    db::prepare_db(&db)?;

    let args: Vec<_> = env::args().collect();
    if args.len() > 2 && args[1] == "--index" {
        let tx = db.transaction()?;

        let mut cnt = 0;
        for entry in WalkDir::new(args[2].clone()) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("Error walking: {}", e);
                    continue;
                }
            };

            let metadata = entry.metadata()?;
            let inode = metadata.ino();
            let Some(parent) = entry
                .path()
                .parent() else {
                continue
            };

            let parent_inode = parent.metadata()?.ino();

            db::insert(&tx, parent_inode, inode, entry.file_name().try_into()?)?;
            cnt += 1;
            if cnt % 1000 == 0 {
                println!("{} {}", cnt, entry.path().display());
            }
        }

        tx.commit()?;
    }

    if args.len() > 2 && args[1] == "--query" {
        let tx = db.transaction()?;
        {

            let segments: Vec<_> = args[2].split("/").collect();
            let (mut query, params) = db::prepare_query(&tx, &segments)?;

            query.query_map(
                rusqlite::params_from_iter(params),
                |x| {
                    Ok(x.get::<usize, i64>(0)?)
                })?.for_each(|x| {
                    let _ = dbg!(x);
                });
        }
        tx.commit()?;
    }

    if args.len() > 2 && args[1] == "--watch" {
        let e = fanotify::watch(&path::PathBuf::from(args[2].clone()), &mut |x| {
            let _ = dbg!(x);
            ops::ControlFlow::Continue(())
        });

        let _ = dbg!(e);
    }

    Ok(())
}
