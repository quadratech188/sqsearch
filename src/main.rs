use std::path;

use clap::Parser;

mod db;
mod fanotify;
mod indexer;
mod watcher;
mod queryer;

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

fn main() -> Result<(), String> {
    env_logger::Builder::from_env(
        env_logger::Env::default()
        .default_filter_or("info")
    ).init();

    let args = Args::parse();

    let flags = match args.command {
        Commands::Watch { filesystem: _ }
            => rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        Commands::Index { dir: _ }
            => rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        Commands::Query
            => rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    };

    let mut db = rusqlite::Connection::open_with_flags(&args.db, flags).map_err(|e|
        e.to_string())?;

    db::prepare_db(&db).map_err(|e| e.to_string())?;

    match args.command {
        Commands::Watch { filesystem } => {
            watcher::watch(&filesystem, &mut db).map_err(|e| e.to_string())?;
        }
        Commands::Index { dir } => {
            indexer::index(&mut db, &dir).map_err(|e| e.to_string())?;
        }
        Commands::Query => {
            queryer::query(db).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}
