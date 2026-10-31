//! SCIP rows in the per-root store. Writing goes through the writer connection (`Store::open`); a reader opened
//! with `Store::open_reader` sees the tables through the same schema because the writer created them.

use std::collections::HashMap;

use rusqlite::{Transaction, params};

use super::Store;
use crate::chunk::ChunkKey;
use crate::scip::{OccurrenceRow, ScipRows};

impl Store {
    /// Replaces every SCIP row in one transaction. `hashes` maps each indexed file to the blake3 of its bytes at
    /// SCIP time; files absent from `rows.files` lose their rows, so a vanished file leaves nothing behind.
    pub fn load_scip(
        &mut self,
        rows: &ScipRows,
        hashes: &HashMap<String, ChunkKey>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM scip_symbols; DELETE FROM scip_occurrences; DELETE FROM scip_files;",
        )?;
        let ids = insert_symbols(&tx, rows)?;
        for occurrence in &rows.occurrences {
            insert_occurrence(&tx, occurrence, &ids)?;
        }
        for file in &rows.files {
            if let Some(hash) = hashes.get(file) {
                tx.execute(
                    "INSERT OR REPLACE INTO scip_files(file, content_hash) VALUES (?1, ?2)",
                    params![file, hash.as_slice()],
                )?;
            }
        }
        tx.commit()
    }

    pub fn scip_counts(&self) -> rusqlite::Result<(u64, u64, u64)> {
        Ok((
            self.count("scip_symbols")?,
            self.count("scip_occurrences")?,
            self.count("scip_files")?,
        ))
    }

    /// The blake3 recorded for `file` at SCIP time, if the file has SCIP rows.
    pub fn scip_hash(&self, file: &str) -> rusqlite::Result<Option<ChunkKey>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT content_hash FROM scip_files WHERE file=?1",
                [file],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(blob.and_then(|bytes| ChunkKey::try_from(bytes).ok()))
    }
}

/// A symbol's definition site: where it is declared and the line span of its whole item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefRow {
    pub symbol_id: i64,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub enclosing: (u32, u32),
}

/// A reference occurrence: where a symbol is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRow {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

const DEF_SELECT: &str = "SELECT s.id, s.display_name, s.kind, o.file, o.start_line, o.start_col, \
     COALESCE(o.enclosing_start, o.start_line), COALESCE(o.enclosing_end, o.end_line) \
     FROM scip_occurrences o JOIN scip_symbols s ON s.id = o.symbol_id WHERE o.role & 1 = 1 AND s.symbol NOT LIKE 'local %'";

/// Kinds that run code. rust-analyzer labels an associated function without `self` (`Walk::new`) `StaticMethod`.
const CALLABLE_KINDS: &str = "'Function','Method','StaticMethod','Constructor'";

fn def_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DefRow> {
    Ok(DefRow {
        symbol_id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        file: row.get(3)?,
        line: row.get(4)?,
        col: row.get(5)?,
        enclosing: (row.get(6)?, row.get(7)?),
    })
}

impl Store {
    /// Definitions whose display name equals `name`, or whose symbol string ends with `name`.
    pub fn scip_defs_named(&self, name: &str) -> rusqlite::Result<Vec<DefRow>> {
        let sql = format!(
            "{DEF_SELECT} AND (s.display_name = ?1 OR s.symbol LIKE '%' || ?1 || '#' OR s.symbol LIKE '%' || ?1 || '().') \
             ORDER BY o.file, o.start_line"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement.query_map([name], def_row)?.collect()
    }

    /// The definition of the symbol occurring at `file:line:col`.
    pub fn scip_def_at(&self, file: &str, line: u32, col: u32) -> rusqlite::Result<Option<DefRow>> {
        let sql = format!(
            "{DEF_SELECT} AND s.id = (SELECT symbol_id FROM scip_occurrences WHERE file=?1 AND start_line=?2 \
             AND start_col <= ?3 AND end_col > ?3 ORDER BY end_col - start_col LIMIT 1) LIMIT 1"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement
            .query_map(params![file, line, col], def_row)?
            .next()
            .transpose()
    }

    /// The first signature line SCIP recorded for the symbol, empty when it recorded none.
    pub fn scip_signature(&self, symbol_id: i64) -> rusqlite::Result<String> {
        let mut statement = self.conn.prepare_cached("SELECT signature_doc FROM scip_symbols WHERE id=?1")?;
        statement.query_row([symbol_id], |row| row.get(0))
    }

    /// Every non-definition occurrence of a symbol, file then line order.
    pub fn scip_refs(&self, symbol_id: i64) -> rusqlite::Result<Vec<RefRow>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT file, start_line, start_col FROM scip_occurrences WHERE symbol_id=?1 AND role & 1 = 0 \
             ORDER BY file, start_line, start_col",
        )?;
        statement
            .query_map([symbol_id], |row| {
                Ok(RefRow {
                    file: row.get(0)?,
                    line: row.get(1)?,
                    col: row.get(2)?,
                })
            })?
            .collect()
    }

    /// The innermost definition whose item span holds `file:line`.
    pub fn scip_enclosing(&self, file: &str, line: u32) -> rusqlite::Result<Option<DefRow>> {
        let sql = format!(
            "{DEF_SELECT} AND o.file=?1 AND o.enclosing_start <= ?2 AND o.enclosing_end >= ?2 \
             AND s.kind IN ({CALLABLE_KINDS},'Struct','Enum','Trait','Impl') \
             ORDER BY o.enclosing_end - o.enclosing_start LIMIT 1"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement
            .query_map(params![file, line], def_row)?
            .next()
            .transpose()
    }

    /// Definitions referenced from inside `def`'s item span, with the call line; same-symbol and local hits dropped.
    pub fn scip_callees(&self, def: &DefRow) -> rusqlite::Result<Vec<(u32, DefRow)>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT DISTINCT o.start_line, o.symbol_id FROM scip_occurrences o WHERE o.file=?1 AND o.role & 1 = 0 \
             AND o.start_line BETWEEN ?2 AND ?3 AND o.symbol_id != ?4 ORDER BY o.start_line",
        )?;
        let hits: Vec<(u32, i64)> = statement
            .query_map(
                params![def.file, def.enclosing.0, def.enclosing.1, def.symbol_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        let mut callees = Vec::new();
        for (line, symbol_id) in hits {
            let sql = format!(
                "{DEF_SELECT} AND s.id = ?1 AND s.kind IN ({CALLABLE_KINDS}) LIMIT 1"
            );
            let mut lookup = self.conn.prepare_cached(&sql)?;
            if let Some(target) = lookup.query_map([symbol_id], def_row)?.next().transpose()? {
                callees.push((line, target));
            }
        }
        Ok(callees)
    }
}

fn insert_symbols(tx: &Transaction<'_>, rows: &ScipRows) -> rusqlite::Result<HashMap<String, i64>> {
    let mut ids = HashMap::new();
    let mut statement = tx.prepare(
        "INSERT OR IGNORE INTO scip_symbols(symbol, display_name, kind, signature_doc) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for symbol in &rows.symbols {
        statement.execute(params![
            symbol.symbol,
            symbol.display_name,
            symbol.kind,
            symbol.signature_doc
        ])?;
    }
    let mut lookup = tx.prepare("SELECT symbol, id FROM scip_symbols")?;
    for pair in lookup.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (symbol, id) = pair?;
        ids.insert(symbol, id);
    }
    Ok(ids)
}

fn insert_occurrence(
    tx: &Transaction<'_>,
    occurrence: &OccurrenceRow,
    ids: &HashMap<String, i64>,
) -> rusqlite::Result<()> {
    let Some(symbol_id) = ids.get(&occurrence.symbol) else {
        return Ok(());
    };
    let (enclosing_start, enclosing_end) = occurrence.enclosing.unzip();
    tx.prepare_cached(
        "INSERT INTO scip_occurrences(file, start_line, start_col, end_line, end_col, symbol_id, role, enclosing_start, enclosing_end) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?
    .execute(params![
        occurrence.file,
        occurrence.start_line,
        occurrence.start_col,
        occurrence.end_line,
        occurrence.end_col,
        symbol_id,
        occurrence.role,
        enclosing_start,
        enclosing_end
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip::SymbolRow;
    use crate::store::StoreConfig;

    fn rows_for(files: &[&str]) -> ScipRows {
        let symbol = "rust-analyzer cargo ss 0.1.0 store/Store#".to_string();
        ScipRows {
            files: files.iter().map(|file| (*file).to_string()).collect(),
            symbols: vec![SymbolRow {
                symbol: symbol.clone(),
                display_name: "Store".into(),
                kind: "Struct".into(),
                signature_doc: "pub struct Store".into(),
            }],
            occurrences: files
                .iter()
                .map(|file| OccurrenceRow {
                    file: (*file).to_string(),
                    start_line: 3,
                    start_col: 11,
                    end_line: 3,
                    end_col: 16,
                    symbol: symbol.clone(),
                    role: 1,
                    enclosing: Some((3, 9)),
                })
                .collect(),
        }
    }

    #[test]
    fn associated_functions_without_self_are_callees_and_owners() {
        let dir = tempfile::tempdir().expect("scratch dir for the store");
        let config = StoreConfig { dir: dir.path().to_path_buf(), model_id: "test-model".into() };
        let mut store = Store::open(&config).expect("open writer");
        let mut rows = ScipRows { files: vec!["lib.rs".into()], ..ScipRows::default() };
        let site = |symbol: &str, line: u32, role: i32, enclosing: Option<(u32, u32)>| OccurrenceRow {
            file: "lib.rs".into(),
            start_line: line,
            start_col: 4,
            end_line: line,
            end_col: 8,
            symbol: symbol.into(),
            role,
            enclosing,
        };
        for (symbol, name, kind) in [
            ("ss impl#[Walk]new().", "new", "StaticMethod"),
            ("ss impl#[Walk]run().", "run", "Method"),
            ("ss top().", "top", "Function"),
        ] {
            let signature_doc = format!("pub fn {name}()");
            rows.symbols.push(SymbolRow { symbol: symbol.into(), display_name: name.into(), kind: kind.into(), signature_doc });
        }
        rows.occurrences = vec![
            site("ss impl#[Walk]new().", 1, 1, Some((1, 1))),
            site("ss impl#[Walk]run().", 2, 1, Some((2, 2))),
            site("ss top().", 4, 1, Some((4, 7))),
            site("ss impl#[Walk]new().", 5, 0, None),
            site("ss impl#[Walk]run().", 5, 0, None),
        ];
        store.load_scip(&rows, &HashMap::new()).expect("load");

        let top = store.scip_defs_named("top").expect("top def").remove(0);
        let names: Vec<String> = store.scip_callees(&top).expect("callees").into_iter().map(|(_, def)| def.name).collect();

        assert_eq!(names, vec!["new".to_string(), "run".to_string()]);
        let owner = store.scip_enclosing("lib.rs", 1).expect("enclosing").expect("new spans line 1");
        assert_eq!(owner.name, "new");
    }

    #[test]
    fn reload_without_a_file_drops_its_rows_and_hash() {
        let dir = tempfile::tempdir().expect("scratch dir for the store");
        let config = StoreConfig {
            dir: dir.path().to_path_buf(),
            model_id: "test-model".into(),
        };
        let mut store = Store::open(&config).expect("open writer");
        let hashes: HashMap<String, ChunkKey> =
            [("a.rs".to_string(), [7; 32]), ("b.rs".to_string(), [9; 32])].into();
        store
            .load_scip(&rows_for(&["a.rs", "b.rs"]), &hashes)
            .expect("first load");
        assert_eq!(store.scip_counts().expect("counts"), (1, 2, 2));
        assert_eq!(store.scip_hash("b.rs").expect("hash"), Some([9; 32]));
        store
            .load_scip(&rows_for(&["a.rs"]), &hashes)
            .expect("second load");
        assert_eq!(store.scip_counts().expect("counts"), (1, 1, 1));
        assert_eq!(store.scip_hash("b.rs").expect("hash"), None);
    }

    #[test]
    fn references_map_to_their_enclosing_definition_and_definitions_resolve_by_name() {
        let dir = tempfile::tempdir().expect("scratch dir for the store");
        let config = StoreConfig {
            dir: dir.path().to_path_buf(),
            model_id: "test-model".into(),
        };
        let mut store = Store::open(&config).expect("open writer");
        let mut rows = rows_for(&["a.rs"]);
        rows.symbols[0].kind = "Function".into();
        rows.occurrences.push(OccurrenceRow {
            start_line: 5,
            enclosing: None,
            role: 0,
            ..rows.occurrences[0].clone()
        });
        store.load_scip(&rows, &HashMap::new()).expect("load");
        let defs = store
            .scip_defs_named("Store")
            .expect("defs by display name");
        assert_eq!(defs.len(), 1);
        let refs = store.scip_refs(defs[0].symbol_id).expect("refs");
        assert_eq!(
            refs,
            vec![RefRow {
                file: "a.rs".into(),
                line: 5,
                col: 11
            }]
        );
        let owner = store
            .scip_enclosing("a.rs", 5)
            .expect("enclosing")
            .expect("a definition spans line 5");
        assert_eq!(owner.name, "Store");
    }
}
