# SQSearch
A barebones file database.
## What this does
- Real-time filesystem monitoring using `fanotify`
- Efficient updates for CREATEs, DELETEs, and MOVEs (Moving a directory with 1000 files doesn't update 1000 entries)
- Substring matching on individual path fragments (Explained below)
- Very simple stdin/stdout protocol - easy to wrap into launchers, etc.
## What this doesn't do
- Keep track of recently accessed files
- Claim to be a replacement of [fsearch](https://github.com/cboxdoerfer/fsearch)
## Querying
Some file search systems only index filenames. Which is fine if you "organize" files like this:
```
stuff
├── chemistry-notes.pdf
├── chemistry-results.md
├── linalg-notes.pdf
├── linalg-results.md
├── physics-notes.pdf
└── physics-results.md
```
But for normal folder structures like this:
```
stuff
├── chemistry
│   ├── notes.pdf
│   └── results.md
├── linalg
│   ├── notes.pdf
│   └── results.md
└── physics
    ├── notes.pdf
    └── results.md
```
The parent directory `chemistry` is just as important as the filename `notes.pdf`. Which is why the (only) query type supported in SQSearch lets you find substrings in both: `chem/not`.

A natural side effect of this method is that the query `chem/` lists all entries in the `chemistry` directory.
## Usage
Index a filesystem:
```
sqsearch --db ./db.sqlite3 index /mnt/data
```
Watch the filesystem for changes:
```
sudo sqsearch --db ./db.sqlite3 watch /mnt/data
```
>[!NOTE]
> `sudo` is unfortunately required to watch entire filesystems.

Query:
```
sqsearch --db ./db.sqlite3 query
```
## Limitations
- Placing the file database in the monitored filesystem itself may trigger unexpected behavior.
- Symlinks are treated as normal files, paths are *not* resolved.
- You can only index entire filesystems, you cannot restrict the database to specific folders.
