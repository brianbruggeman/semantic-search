//! Read side of the code graph: plain row lookups. Resolution (how many definitions a written name could mean) is
//! decided by the caller from these rows, never stored.

use rusqlite::{Row, params};

use super::Store;
use crate::chunk::ChunkKey;
use crate::graph::{EdgeKind, GraphEdge, GraphNode, NodeKind};

/// A stored symbol and the file that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub file: String,
    pub node: GraphNode,
}

/// A stored relation and the file whose source states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub file: String,
    pub edge: GraphEdge,
}

const NODE_COLUMNS: &str = "file, kind, name, qpath, sig, doc, start_line, end_line, chunk_key";
const EDGE_COLUMNS: &str = "file, kind, src, dst, qual, line";

fn kind_of(text: &str) -> NodeKind {
    [
        NodeKind::File,
        NodeKind::Module,
        NodeKind::Struct,
        NodeKind::Enum,
        NodeKind::Trait,
        NodeKind::Type,
        NodeKind::Function,
        NodeKind::Method,
        NodeKind::Const,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == text)
    .unwrap_or(NodeKind::Function)
}

fn edge_kind_of(text: &str) -> EdgeKind {
    [
        EdgeKind::Contains,
        EdgeKind::Imports,
        EdgeKind::Calls,
        EdgeKind::Implements,
        EdgeKind::Extends,
        EdgeKind::Tests,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == text)
    .unwrap_or(EdgeKind::Calls)
}

fn node_row(row: &Row<'_>) -> rusqlite::Result<NodeRow> {
    let kind: String = row.get(1)?;
    let key: Option<Vec<u8>> = row.get(8)?;
    Ok(NodeRow {
        file: row.get(0)?,
        node: GraphNode {
            kind: kind_of(&kind),
            name: row.get(2)?,
            qpath: row.get(3)?,
            signature: row.get(4)?,
            doc: row.get(5)?,
            start_line: row.get(6)?,
            end_line: row.get(7)?,
            chunk_key: key.and_then(|bytes| ChunkKey::try_from(bytes).ok()),
        },
    })
}

fn edge_row(row: &Row<'_>) -> rusqlite::Result<EdgeRow> {
    let kind: String = row.get(1)?;
    Ok(EdgeRow {
        file: row.get(0)?,
        edge: GraphEdge {
            kind: edge_kind_of(&kind),
            src: row.get(2)?,
            dst: row.get(3)?,
            qual: row.get(4)?,
            line: row.get(5)?,
        },
    })
}

fn kind_list(kinds: &[EdgeKind]) -> String {
    kinds
        .iter()
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

impl Store {
    /// The symbol a chunk is about: the first non-module symbol that starts inside its line range.
    pub fn primary_node(
        &self,
        file: &str,
        start: u32,
        end: u32,
    ) -> rusqlite::Result<Option<NodeRow>> {
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE file=?1 AND kind NOT IN ('file','module') \
             AND start_line BETWEEN ?2 AND ?3 ORDER BY start_line, id LIMIT 1"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut rows = statement.query_map(params![file, start, end], node_row)?;
        rows.next().transpose()
    }

    /// The innermost symbol whose line range holds `line`.
    pub fn node_at(&self, file: &str, line: u32) -> rusqlite::Result<Option<NodeRow>> {
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE file=?1 AND kind NOT IN ('file','module') \
             AND start_line <= ?2 AND end_line >= ?2 ORDER BY end_line - start_line, id LIMIT 1"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut rows = statement.query_map(params![file, line], node_row)?;
        rows.next().transpose()
    }

    /// Every symbol called `name`, files and modules excluded.
    pub fn nodes_named(&self, name: &str) -> rusqlite::Result<Vec<NodeRow>> {
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE name=?1 AND kind!='file' ORDER BY file, start_line"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement.query_map([name], node_row)?.collect()
    }

    /// Symbols whose `qpath` is exactly `container::<one segment>` in `file`: the direct members of a container.
    pub fn members_of(&self, file: &str, container: &str) -> rusqlite::Result<Vec<NodeRow>> {
        let width = i64::try_from(container.chars().count()).unwrap_or(0);
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE file=?1 AND kind!='file' \
             AND substr(qpath, 1, ?2) = ?3 AND substr(qpath, ?2 + 1, 2) = '::' \
             AND instr(substr(qpath, ?2 + 3), '::') = 0 ORDER BY start_line, id"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement
            .query_map(params![file, width, container], node_row)?
            .collect()
    }

    pub fn node_by_qpath(&self, file: &str, qpath: &str) -> rusqlite::Result<Option<NodeRow>> {
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE file=?1 AND qpath=?2 ORDER BY id LIMIT 1"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut rows = statement.query_map(params![file, qpath], node_row)?;
        rows.next().transpose()
    }

    /// Edges of the given kinds that leave `src` in `file`, in source order.
    pub fn edges_from(
        &self,
        file: &str,
        src: &str,
        kinds: &[EdgeKind],
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM edges WHERE file=?1 AND src=?2 AND kind IN ({}) ORDER BY line, id",
            kind_list(kinds)
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement.query_map(params![file, src], edge_row)?.collect()
    }

    /// Edges of the given kinds whose written destination is `dst`, anywhere in the index.
    pub fn edges_to(&self, dst: &str, kinds: &[EdgeKind]) -> rusqlite::Result<Vec<EdgeRow>> {
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM edges WHERE dst=?1 AND kind IN ({}) ORDER BY file, line, id",
            kind_list(kinds)
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement.query_map([dst], edge_row)?.collect()
    }

    /// The symbols that sit at `qpath` among all files: used to find where a typed container lives.
    pub fn symbols_matching(&self, text: &str, limit: usize) -> rusqlite::Result<Vec<NodeRow>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE kind!='file' AND (name=?1 OR qpath=?1 \
             OR (length(qpath) > length(?1) AND substr(qpath, -length(?1) - 2) = '::' || ?1)) \
             ORDER BY (name=?1) DESC, file, start_line LIMIT ?2"
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        statement
            .query_map(params![text, limit], node_row)?
            .collect()
    }
}
