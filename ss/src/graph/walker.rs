//! The language-neutral tree walk: each language classifies a syntax node into an `Item`, this module turns items
//! into nodes and edges while tracking the qualified-path scope.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{
    EdgeKind, FileGraph, GraphEdge, GraphNode, NodeKind, file_module, golang, python, rust, script,
};
use crate::chunk::Language;

const SIGNATURE_CHARS: usize = 160;

pub struct Ctx<'s> {
    pub src: &'s str,
    pub rel: &'s str,
    pub language: Language,
}

impl<'s> Ctx<'s> {
    pub fn text(&self, node: Node<'_>) -> &'s str {
        self.src.get(node.byte_range()).unwrap_or("")
    }

    pub fn field(&self, node: Node<'_>, name: &str) -> &'s str {
        node.child_by_field_name(name)
            .map_or("", |child| self.text(child))
    }
}

/// A definition found at `span`: the walk descends into `body` with the new symbol as scope.
pub struct Def<'t> {
    pub kind: NodeKind,
    pub name: String,
    pub span: Node<'t>,
    pub body: Option<Node<'t>>,
    pub heritage: Vec<(EdgeKind, String)>,
    pub parent: Option<String>,
    pub test: bool,
}

pub enum Item<'t> {
    Def(Def<'t>),
    Impl {
        ty: String,
        tr: Option<String>,
        body: Option<Node<'t>>,
    },
    Imports(Vec<(String, String)>),
    Call {
        dst: String,
        qual: String,
        opens_test: bool,
    },
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Within {
    Namespace,
    Type,
    Callable,
}

#[derive(Clone)]
struct Scope {
    qpath: String,
    owner: String,
    within: Within,
    test: bool,
}

#[must_use]
pub fn extract_tree(language: Language, rel: &str, text: &str) -> FileGraph {
    let mut parser = Parser::new();
    let tree = parser
        .set_language(&language.grammar())
        .ok()
        .and_then(|()| parser.parse(text, None));
    let Some(tree) = tree else {
        return FileGraph::default();
    };
    let ctx = Ctx {
        src: text,
        rel,
        language,
    };
    let module = file_module(rel, language);
    let scope = Scope {
        qpath: module,
        owner: rel.to_owned(),
        within: Within::Namespace,
        test: false,
    };
    let mut walker = Walker::new(&ctx);
    walker.walk(tree.root_node(), &scope);
    FileGraph {
        nodes: walker.nodes,
        edges: walker.edges,
    }
}

fn classify<'t>(ctx: &Ctx<'_>, node: Node<'t>, scope_is_type: bool) -> Item<'t> {
    match ctx.language {
        Language::Rust => rust::classify(ctx, node),
        Language::Python => python::classify(ctx, node),
        Language::Go => golang::classify(ctx, node),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            script::classify(ctx, node, scope_is_type)
        }
    }
}

fn doc_of(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    match ctx.language {
        Language::Rust => rust::doc(ctx, node),
        Language::Python => python::doc(ctx, node),
        Language::Go => golang::doc(ctx, node),
        Language::TypeScript | Language::Tsx | Language::JavaScript => script::doc(ctx, node),
    }
}

struct Walker<'c, 's> {
    ctx: &'c Ctx<'s>,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    seen: HashSet<GraphEdge>,
}

impl<'c, 's> Walker<'c, 's> {
    fn new(ctx: &'c Ctx<'s>) -> Self {
        Self {
            ctx,
            nodes: Vec::new(),
            edges: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn walk(&mut self, node: Node<'_>, scope: &Scope) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match classify(self.ctx, child, scope.within == Within::Type) {
                Item::Def(def) => self.define(&def, scope),
                Item::Impl { ty, tr, body } => self.implement(&ty, tr, body, scope),
                Item::Imports(names) => self.imports(names, child),
                Item::Call {
                    dst,
                    qual,
                    opens_test,
                } => {
                    self.call(&dst, &qual, child, scope);
                    let inner = Scope {
                        test: scope.test || opens_test,
                        ..scope.clone()
                    };
                    self.walk(child, &inner);
                }
                Item::Other => self.walk(child, scope),
            }
        }
    }

    fn edge(&mut self, kind: EdgeKind, src: &str, dst: &str, qual: &str, line: u32) {
        let edge = GraphEdge {
            kind,
            src: src.to_owned(),
            dst: dst.to_owned(),
            qual: qual.to_owned(),
            line,
        };
        let identity = GraphEdge {
            line: 0,
            ..edge.clone()
        };
        if self.seen.insert(identity) {
            self.edges.push(edge);
        }
    }

    fn module(&self) -> String {
        file_module(self.ctx.rel, self.ctx.language)
    }

    fn define(&mut self, def: &Def<'_>, scope: &Scope) {
        let base = def.parent.as_ref().map_or_else(
            || scope.qpath.clone(),
            |parent| format!("{}::{parent}", self.module()),
        );
        let qpath = format!("{base}::{}", def.name);
        let kind = if def.kind == NodeKind::Function
            && (scope.within == Within::Type || def.parent.is_some())
        {
            NodeKind::Method
        } else {
            def.kind
        };
        let line = line_of(def.span.start_position().row);
        let src = if base == self.module() {
            self.ctx.rel.to_owned()
        } else {
            base.clone()
        };
        self.edge(EdgeKind::Contains, &src, &qpath, &def.name, line);
        for (heritage, name) in &def.heritage {
            self.edge(*heritage, &qpath, name, "", line);
        }
        self.nodes.push(GraphNode {
            kind,
            name: def.name.clone(),
            qpath: qpath.clone(),
            signature: signature(self.ctx, def.span, def.body),
            doc: doc_of(self.ctx, def.span),
            start_line: line,
            end_line: line_of(def.span.end_position().row),
            chunk_key: None,
        });
        if let Some(body) = def.body {
            let inner = Scope {
                qpath: qpath.clone(),
                owner: qpath,
                within: if kind.callable() {
                    Within::Callable
                } else {
                    Within::Type
                },
                test: scope.test || def.test,
            };
            self.walk(body, &inner);
        }
    }

    fn implement(&mut self, ty: &str, tr: Option<String>, body: Option<Node<'_>>, scope: &Scope) {
        let qpath = format!("{}::{ty}", scope.qpath);
        if let Some(name) = tr {
            let line = body.map_or(0, |node| line_of(node.start_position().row));
            self.edge(EdgeKind::Implements, &qpath, &name, "", line);
        }
        if let Some(body) = body {
            let inner = Scope {
                qpath: qpath.clone(),
                owner: qpath,
                within: Within::Type,
                test: scope.test,
            };
            self.walk(body, &inner);
        }
    }

    fn imports(&mut self, names: Vec<(String, String)>, node: Node<'_>) {
        let line = line_of(node.start_position().row);
        let rel = self.ctx.rel;
        for (dst, qual) in names {
            self.edge(EdgeKind::Imports, rel, &dst, &qual, line);
        }
    }

    fn call(&mut self, dst: &str, qual: &str, node: Node<'_>, scope: &Scope) {
        let kind = if scope.test {
            EdgeKind::Tests
        } else {
            EdgeKind::Calls
        };
        let line = line_of(node.start_position().row);
        self.edge(kind, &scope.owner, dst, qual, line);
    }
}

fn line_of(row: usize) -> u32 {
    u32::try_from(row + 1).unwrap_or(u32::MAX)
}

fn signature(ctx: &Ctx<'_>, span: Node<'_>, body: Option<Node<'_>>) -> String {
    let end = body.map_or(span.end_byte(), |node| node.start_byte());
    let head = ctx.src.get(span.start_byte()..end).unwrap_or("");
    let head = if body.is_none() {
        head.lines().next().unwrap_or("")
    } else {
        head
    };
    cap(&head.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn cap(text: &str) -> String {
    text.chars().take(SIGNATURE_CHARS).collect()
}
