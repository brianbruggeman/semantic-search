use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Version of the structured reply shapes below; bumped on any incompatible change to a field's meaning or name.
pub const SCHEMA_VERSION: u32 = 1;

/// How many definitions in the index a name-only edge could mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolved {
    Exact,
    Ambiguous,
    Unresolved,
}

/// A definition: `kind` is `fn`, `method`, `struct`, `enum`, `trait`, `type`, `const` or `module`; `qpath` is `module::Type::method`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub kind: String,
    pub name: String,
    pub qpath: String,
    pub signature: String,
    pub doc: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub chunk_key: String,
}

/// A symbol listed beside a hit: a member of its container, or a test that exercises it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub kind: String,
    pub name: String,
    pub signature: String,
    pub path: String,
    pub line: u32,
}

/// One relation seen from a symbol. For a caller, `path:line` is the call site; for a callee it is the definition
/// (the first candidate when ambiguous) or, when unresolved, the call site. `depth` is 1 for direct, 2 for the next hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRef {
    pub name: String,
    pub path: String,
    pub line: u32,
    pub resolved: Resolved,
    pub depth: u8,
}

/// A list capped at a fixed length: the kept entries and how many were left out or not listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capped<Item> {
    pub items: Vec<Item>,
    /// Entries past the cap that were left out.
    pub more: u32,
    /// Entries that resolve to nothing in the index (std, other crates, locals) and are not listed.
    pub hidden: u32,
}

impl<Item> Default for Capped<Item> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            more: 0,
            hidden: 0,
        }
    }
}

/// One ranked hit with the symbol it falls in and that symbol's neighbours; `symbol` is empty for chunks with none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub rank: u32,
    pub score: f64,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub chunk_key: String,
    pub preview: Vec<String>,
    pub symbol: Option<Symbol>,
    pub container: String,
    pub members: Capped<Member>,
    pub calls: Capped<EdgeRef>,
    pub called_by: Capped<EdgeRef>,
    pub tests: Capped<EdgeRef>,
}

/// The hits for one query of a multi-query call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Facet {
    pub query: String,
    pub records: Vec<Record>,
}

/// A symbol next to several hits that is not a hit itself; `relation` is `calls`, `called_by`, `member` or `tests`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Related {
    pub name: String,
    pub path: String,
    pub line: u32,
    pub relation: String,
    pub mentions: u32,
}

/// The reply to `Request::Query`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Findings {
    pub schema: u32,
    pub facets: Vec<Facet>,
    pub related: Capped<Related>,
}

/// A symbol with its neighbourhood; `callers`/`callees` hold depth 1 entries and, when asked, depth 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Neighborhood {
    pub symbol: Symbol,
    pub members: Capped<Member>,
    pub callers: Capped<EdgeRef>,
    pub callees: Capped<EdgeRef>,
    pub importers: Capped<EdgeRef>,
    pub tests: Capped<EdgeRef>,
}

/// The reply to `Request::Graph`: every symbol the target named (at most five), `more` counts the rest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphReply {
    pub schema: u32,
    pub neighborhoods: Capped<Neighborhood>,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;

    use super::*;
    use crate::{Frame, LineDecoder, Request, Response, encode_frame};

    fn sample_record() -> Record {
        Record {
            rank: 1,
            score: 0.0328,
            path: "/work/ss/src/query.rs".into(),
            start_line: 17,
            end_line: 30,
            chunk_key: "ab".repeat(32),
            preview: Vec::new(),
            symbol: Some(Symbol {
                kind: "fn".into(),
                name: "rrf_fuse".into(),
                qpath: "query::rrf_fuse".into(),
                signature: "pub fn rrf_fuse(rankings: &[Vec<i64>]) -> Vec<(i64, f64)>".into(),
                doc: "Reciprocal rank fusion of best-first id lists".into(),
                path: "/work/ss/src/query.rs".into(),
                start_line: 17,
                end_line: 30,
                chunk_key: "ab".repeat(32),
            }),
            container: "mod query".into(),
            members: Capped::default(),
            calls: Capped {
                items: vec![EdgeRef {
                    name: "entry".into(),
                    path: "/work/ss/src/query.rs".into(),
                    line: 20,
                    resolved: Resolved::Ambiguous,
                    depth: 1,
                }],
                more: 3,
                hidden: 9,
            },
            called_by: Capped::default(),
            tests: Capped::default(),
        }
    }

    #[test]
    fn a_findings_reply_survives_the_json_line_round_trip() {
        let findings = Findings {
            schema: SCHEMA_VERSION,
            facets: vec![Facet {
                query: "fuse rankings".into(),
                records: vec![sample_record()],
            }],
            related: Capped::default(),
        };
        let mut bytes = alloc::vec::Vec::new();
        encode_frame(
            &Frame {
                id: 7,
                body: Response::Findings {
                    findings: findings.clone(),
                },
            },
            &mut bytes,
        )
        .expect("encode");
        let mut decoder = LineDecoder::new();
        decoder.push(&bytes);

        let frame = decoder
            .next_frame::<Response>()
            .expect("a frame")
            .expect("well formed");

        assert_eq!(frame.body, Response::Findings { findings });
    }

    #[test]
    fn query_and_graph_requests_use_snake_case_op_tags() {
        let query = serde_json::to_string(&Request::Query {
            queries: vec!["a b".into()],
            path: "/w".into(),
            limit: 5,
        })
        .expect("encode");
        let graph = serde_json::to_string(&Request::Graph {
            target: "rrf_fuse".into(),
            path: "/w".into(),
            depth: 2,
        })
        .expect("encode");

        assert!(query.starts_with(r#"{"op":"query""#), "{query}");
        assert!(graph.starts_with(r#"{"op":"graph""#), "{graph}");
    }
}
