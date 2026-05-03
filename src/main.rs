use std::{error, path};

use clap::Parser;

mod db;
mod fanotify;
mod indexer;
mod watcher;
mod queryer;

#[derive(clap::Parser, Debug)]
struct CLI {
    #[command(flatten)]
    globals: GlobalArgs,
    #[command(subcommand)]
    command: Commands
}

#[derive(clap::Args, Debug)]
struct GlobalArgs {
    #[arg(long, value_name = "FILE", default_value="/var/lib/sqsearch/db.sqlite3")]
    db: path::PathBuf,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Commands {
    Watch(WatchArgs),
    Index(IndexArgs),
    Query(QueryArgs)
}

#[derive(clap::Args, Debug, Clone)]
struct WatchArgs {
    path: path::PathBuf
}

fn watch(globals: GlobalArgs, args: WatchArgs) -> Result<(), anyhow::Error> {
    let mut conn = rusqlite::Connection::open_with_flags(
        globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    )?;

    db::prepare_db(&conn)?;
    watcher::watch(&args.path, &mut conn)?;

    Ok(())
}

#[derive(clap::Args, Debug, Clone)]
struct IndexArgs {
    path: path::PathBuf
}

fn index(globals: GlobalArgs, args: IndexArgs) -> Result<(), anyhow::Error> {
    let mut conn = rusqlite::Connection::open_with_flags(
        globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
    )?;

    db::prepare_db(&conn)?;
    indexer::index(&mut conn, &args.path)?;

    Ok(())
}

#[derive(clap::Args, Debug, Clone)]
struct QueryArgs {

}

fn query(globals: GlobalArgs, _: QueryArgs) -> Result<(), anyhow::Error> {
    let conn = rusqlite::Connection::open_with_flags(
        globals.db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    )?;

    db::prepare_db(&conn)?;
    queryer::query(conn)?;

    Ok(())
}

fn main() -> Result<(), anyhow::Error> {
    env_logger::Builder::from_env(
        env_logger::Env::default()
        .default_filter_or("info")
    ).init();

    let args = CLI::parse();

    match args.command {
        Commands::Watch(x) => watch(args.globals, x),
        Commands::Index(x) => index(args.globals, x),
        Commands::Query(x) => query(args.globals, x)
    }
}
