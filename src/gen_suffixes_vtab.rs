use std::{ffi, mem, os::unix::ffi::OsStrExt};

use rusqlite::vtab::Module;

#[repr(C)]
pub struct SuffixesVtab {
    base: rusqlite::vtab::sqlite3_vtab
}

const COLUMN_SUFFIX: ffi::c_int = 0;
const COLUMN_STRING: ffi::c_int = 1;

unsafe impl rusqlite::vtab::VTab<'_> for SuffixesVtab {
    type Aux = ();
    type Cursor = SuffixesVtabCursor;

    fn connect(
        db: &mut rusqlite::vtab::VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> rusqlite::Result<(std::borrow::Cow<'static, std::ffi::CStr>, Self)>
    {
        db.config(rusqlite::vtab::VTabConfig::Innocuous)?;

        let declaration = c"
            CREATE TABLE x(suffix, string hidden);
        ";

        Ok((declaration.into(), unsafe {mem::zeroed()}))
    }

    fn best_index(&self, info: &mut rusqlite::vtab::IndexInfo) -> rusqlite::Result<bool> {
        let str_constraint = info.constraints_and_usages()
            .filter(|(c, _)|
                c.column() == COLUMN_STRING
                &&
                c.operator() == rusqlite::vtab::IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
                &&
                c.is_usable())
            .next();

        let Some((_constraint, mut usage)) = str_constraint else {return Ok(false)};

        usage.set_argv_index(1);
        usage.set_omit(true);

        Ok(true)
    }

    fn open(&mut self) -> rusqlite::Result<Self::Cursor> {
        unsafe {mem::zeroed()}
    }
}

#[repr(C)]
pub struct SuffixesVtabCursor {
    base: rusqlite::vtab::sqlite3_vtab_cursor,

    osstring: ffi::OsString,
    string: String,
    char_indices: Vec<usize>,
    index: usize
}

unsafe impl rusqlite::vtab::VTabCursor for SuffixesVtabCursor {
    fn filter(
        &mut self, _idx_num: std::ffi::c_int, _idx_str: Option<&str>,
        args: &rusqlite::vtab::Filters<'_>
    ) -> rusqlite::Result<()> {
        let arg = unsafe {ffi::OsString::from_encoded_bytes_unchecked(args.get(0)?)};

        self.osstring = arg.clone();
        self.string = arg.to_string_lossy().to_string();
        self.char_indices = self.string.char_indices()
            .map(|(a, _)| a)
            .collect();
        self.index = 0;

        Ok(())
    }

    fn next(&mut self) -> rusqlite::Result<()> {
        self.index += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.index == self.char_indices.len()
    }

    fn column(&self, ctx: &mut rusqlite::vtab::Context, i: std::ffi::c_int) -> rusqlite::Result<()> {
        match i {
            COLUMN_STRING => {
                ctx.set_result(&self.osstring.as_bytes())
            }
            _ => {
                let suffix = &self.string[self.char_indices[self.index]..];
                ctx.set_result(&suffix)
            }
        }
    }

    fn rowid(&self) -> rusqlite::Result<i64> {
        Ok(self.index as i64)
    }
}

pub const MODULE: rusqlite::vtab::Module<SuffixesVtab> = Module::eponymous_only_module();

#[cfg(test)]
mod tests {

use super::*;

    fn assert_suffixes(
        conn: &rusqlite::Connection,
        keyword: ffi::OsString,
        suffixes: Vec<&str>
    ) -> anyhow::Result<()> {
        let mut stmt = conn.prepare_cached("
            SELECT * from suffixes(?1)
        ")?;

        let results: Result<Vec<String>, _> = stmt.query_map(
            (keyword.as_bytes(),),
            |row| row.get(0)
        )?.collect();

        assert_eq!(suffixes, results?);

        Ok(())
    }

    #[test]
    fn list_suffixes() -> anyhow::Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.create_module("suffixes", &MODULE, None)?;

        assert_suffixes(&conn, "test".into(), vec!["test", "est", "st", "t"])?;
        assert_suffixes(&conn, "한국어".into(), vec!["한국어", "국어", "어"])?;

        Ok(())
    }

    #[test]
    fn joins() -> anyhow::Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.create_module("gen_suffixes", &MODULE, None)?;

        conn.execute("
            CREATE TABLE suffixes (
                suffix TEXT,
                string BLOB
            )
        ", ())?;

        let mut stmt = conn.prepare_cached("
            INSERT INTO suffixes (suffix, string)
            SELECT gen_suffixes.suffix, ?1
            FROM gen_suffixes(?1)
        ")?;

        stmt.execute(("test".as_bytes(),))?;
        stmt.execute(("한국어".as_bytes(),))?;

        let mut stmt = conn.prepare_cached("
            SELECT * from suffixes
        ")?;

        let results: Result<Vec<(String, ffi::OsString)>, _> = stmt.query_map(
            (),
            |row| Ok((row.get(0)?, unsafe {ffi::OsString::from_encoded_bytes_unchecked(row.get(1)?)}))
        )?.collect();
        let mut results = results?;
        results.sort();

        let mut expected: Vec<(String, ffi::OsString)> = vec![
            ("test".into(), "test".into()),
            ("est".into(), "test".into()),
            ("st".into(), "test".into()),
            ("t".into(), "test".into()),
            ("한국어".into(), "한국어".into()),
            ("국어".into(), "한국어".into()),
            ("어".into(), "한국어".into()),
        ];
        expected.sort();

        assert_eq!(results, expected);

        Ok(())
    }
}
