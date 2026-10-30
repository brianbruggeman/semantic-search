//! TypeScript, TSX and JavaScript: function/class/interface/enum/type-alias declarations, `method_definition`,
//! arrow functions bound by `const`, `import_statement` clauses, `call_expression`/`new_expression` callees,
//! `extends`/`implements` heritage. `it`/`test`/`describe` callbacks open a test scope.

use tree_sitter::Node;

use super::walker::{Ctx, Def, Item, cap};
use super::{EdgeKind, METHOD, NodeKind, last_segment};

const TEST_CALLS: [&str; 3] = ["it", "test", "describe"];

pub fn classify<'t>(ctx: &Ctx<'_>, node: Node<'t>, in_type: bool) -> Item<'t> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            define(ctx, node, NodeKind::Function, Vec::new())
        }
        "method_definition" | "method_signature" => define(ctx, node, NodeKind::Method, Vec::new()),
        "class_declaration" | "abstract_class_declaration" => {
            define(ctx, node, NodeKind::Type, heritage(ctx, node))
        }
        "interface_declaration" => define(ctx, node, NodeKind::Trait, heritage(ctx, node)),
        "enum_declaration" => define(ctx, node, NodeKind::Enum, Vec::new()),
        "type_alias_declaration" => define(ctx, node, NodeKind::Type, Vec::new()),
        "lexical_declaration" | "variable_declaration" => binding(ctx, node, in_type),
        "import_statement" => Item::Imports(imports(ctx, node)),
        "call_expression" => call(ctx, node),
        "new_expression" => construct(ctx, node),
        _ => Item::Other,
    }
}

fn define<'t>(
    ctx: &Ctx<'_>,
    node: Node<'t>,
    kind: NodeKind,
    heritage: Vec<(EdgeKind, String)>,
) -> Item<'t> {
    let name = ctx.field(node, "name");
    if name.is_empty() {
        return Item::Other;
    }
    Item::Def(Def {
        kind,
        name: name.trim_matches(['"', '\'']).to_owned(),
        span: node,
        body: node.child_by_field_name("body"),
        heritage,
        parent: None,
        test: kind.callable() && name.starts_with("test"),
    })
}

fn binding<'t>(ctx: &Ctx<'_>, node: Node<'t>, in_type: bool) -> Item<'t> {
    let mut cursor = node.walk();
    let declarator = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "variable_declarator");
    let Some(declarator) = declarator else {
        return Item::Other;
    };
    let name = ctx.field(declarator, "name");
    let value = declarator.child_by_field_name("value");
    let function = value.filter(|found| {
        matches!(
            found.kind(),
            "arrow_function" | "function_expression" | "function"
        )
    });
    let upper = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
    let kind = match (function, upper) {
        (Some(_), _) if !in_type => NodeKind::Function,
        (None, true) => NodeKind::Const,
        _ => return Item::Other,
    };
    Item::Def(Def {
        kind,
        name: name.to_owned(),
        span: node,
        body: function.and_then(|found| found.child_by_field_name("body")),
        heritage: Vec::new(),
        parent: None,
        test: kind.callable() && name.starts_with("test"),
    })
}

fn heritage(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(EdgeKind, String)> {
    let mut found = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_heritage" | "extends_type_clause" => {
                clause_edges(ctx, child, EdgeKind::Extends, &mut found);
            }
            _ => {}
        }
    }
    found
}

fn clause_edges(
    ctx: &Ctx<'_>,
    clause: Node<'_>,
    default: EdgeKind,
    out: &mut Vec<(EdgeKind, String)>,
) {
    let mut cursor = clause.walk();
    for child in clause.named_children(&mut cursor) {
        match child.kind() {
            "extends_clause" => clause_edges(ctx, child, EdgeKind::Extends, out),
            "implements_clause" => clause_edges(ctx, child, EdgeKind::Implements, out),
            "identifier" | "type_identifier" | "member_expression" | "nested_type_identifier" => {
                out.push((default, last_segment(ctx.text(child)).to_owned()));
            }
            _ => {}
        }
    }
}

fn imports(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(String, String)> {
    let source = ctx
        .field(node, "source")
        .trim_matches(['"', '\''])
        .to_owned();
    let mut cursor = node.walk();
    let clause = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "import_clause");
    let Some(clause) = clause else {
        return vec![(last_segment(&source).to_owned(), source)];
    };
    let mut names = Vec::new();
    let mut inner = clause.walk();
    for child in clause.named_children(&mut inner) {
        match child.kind() {
            "identifier" => names.push((ctx.text(child).to_owned(), source.clone())),
            "namespace_import" => names.push(("*".to_owned(), source.clone())),
            "named_imports" => named_specifiers(ctx, child, &source, &mut names),
            _ => {}
        }
    }
    names
}

fn named_specifiers(ctx: &Ctx<'_>, list: Node<'_>, source: &str, out: &mut Vec<(String, String)>) {
    let mut cursor = list.walk();
    for specifier in list.named_children(&mut cursor) {
        let name = ctx.field(specifier, "name");
        if !name.is_empty() {
            out.push((name.to_owned(), source.to_owned()));
        }
    }
}

fn call<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(function) = node.child_by_field_name("function") else {
        return Item::Other;
    };
    let (dst, qual) = match function.kind() {
        "identifier" => (ctx.text(function).to_owned(), String::new()),
        "member_expression" => (
            ctx.field(function, "property").to_owned(),
            receiver(ctx, function),
        ),
        _ => return Item::Other,
    };
    let opens_test = qual.is_empty() && TEST_CALLS.contains(&dst.as_str());
    Item::Call {
        dst,
        qual,
        opens_test,
    }
}

fn construct<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let name = ctx.field(node, "constructor");
    if name.is_empty() {
        return Item::Other;
    }
    Item::Call {
        dst: last_segment(name).to_owned(),
        qual: String::new(),
        opens_test: false,
    }
}

fn receiver(ctx: &Ctx<'_>, member: Node<'_>) -> String {
    let object = member.child_by_field_name("object");
    match object {
        Some(node) if node.kind() == "this" => "self".to_owned(),
        Some(node) if node.kind() == "identifier" => ctx.text(node).to_owned(),
        _ => METHOD.to_owned(),
    }
}

pub fn doc(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    let anchor = node
        .parent()
        .filter(|up| up.kind() == "export_statement")
        .unwrap_or(node);
    let Some(comment) = anchor
        .prev_sibling()
        .filter(|found| found.kind() == "comment")
    else {
        return String::new();
    };
    let line = ctx
        .text(comment)
        .lines()
        .map(|raw| {
            raw.trim()
                .trim_start_matches("/**")
                .trim_start_matches("//")
                .trim_start_matches('*')
                .trim_end_matches("*/")
                .trim()
        })
        .find(|text| !text.is_empty());
    cap(line.unwrap_or(""))
}
