use std::{env, fs, ops, os::{linux::fs::MetadataExt}, path, process};

use clap::Parser;
use walkdir::WalkDir;

mod db;
mod fanotify;
mod watcher;

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(long, value_name = "FILE")]
    db: path::PathBuf,

    #[command(subcommand)]
    command: Commands
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    Watch {
        filesystem: path::PathBuf
    },
    Index {
        dir: path::PathBuf
    },
    Query
}

fn index(conn: &mut rusqlite::Connection, path: &path::Path) -> Result<(), String> {
    let mut tx = conn.transaction().map_err(|e| e.to_string())?;

    let root_metadata = path.metadata().map_err(|e| {
        format!("Failed to get metadata of {}: {}", path.display(), e.to_string())
    })?;

    for (i, entry) in WalkDir::new(path).into_iter().enumerate() {
        let Ok(entry) = entry else {continue};

        let path = entry.path();
        // If symlink, use metadata of target
        let Ok(metadata) = path.metadata() else {
            log::warn!("Failed to get metadata of {}", path.display());
            continue
        };
        if metadata.st_dev() != root_metadata.st_dev() {
            log::debug!("Skipping {} as it is on a different device to root", path.display());
            continue
        }

        let Some(parent) = path.parent() else {continue};
        let Ok(parent_metadata) = parent.metadata() else {
            log::warn!("Failed to get metadata of {}", path.display());
            continue
        };
        let Some(Ok(filename)) = path.file_name().map(|x| x.try_into()) else {continue};

        metadata.st_ino();
        match db::create(&tx, parent_metadata.st_ino(), metadata.st_ino(), filename) {
            Err(db::Error::DuplicateFile(_, _, _)) => {
                log::debug!("File already in DB: {}", path.display());
                Ok(())
            }
            x => x
        }.map_err(|e| e.to_string())?;

        if i % 1000 == 0 {
            log::info!("{} {}", i, path.display());
        }
        if i % 10000 == 0 {
            log::info!("Commiting!");
            let _ = tx.commit().map_err(|e| e.to_string());
            tx = conn.transaction().map_err(|e| e.to_string())?;
            log::info!("Commited!");
        }
    }

    tx.commit().map_err(|e| e.to_string())?;

    Ok(())
}

fn main() -> Result<(), String> {
    env_logger::init();

    let args = Args::parse();

    let mut db = rusqlite::Connection::open(&args.db).map_err(|e|
        e.to_string())?;

    db::prepare_db(&db).map_err(|e| e.to_string())?;

    match args.command {
        Commands::Watch { filesystem } => {
            watcher::watch(&filesystem, &mut db).map_err(|e| e.to_string())?;
        }
        Commands::Index { dir } => {
            index(&mut db, &dir)?;
        }
        Commands::Query => {
            todo!()
        }
    }

    Ok(())

    /*

    let path = path::Path::new("/home/quadratech/.local/share/sqsearch/db.sqlite3");
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

            db::create(&tx, parent_inode, inode, entry.file_name().try_into()?)?;
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
        let path = path::PathBuf::from(args[2].clone());

        let e = watcher::watch(&path, &mut db);

        let _ = dbg!(e);
    }

    Ok(())
    */
}
