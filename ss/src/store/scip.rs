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
}
