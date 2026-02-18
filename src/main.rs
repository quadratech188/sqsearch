use std::path;

use clap::Parser;

mod db;
mod fanotify;
mod indexer;
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
            indexer::index(&mut db, &dir).map_err(|e| e.to_string())?;
        }
        Commands::Query => {
            todo!()
        }
    }

    Ok(())
}
