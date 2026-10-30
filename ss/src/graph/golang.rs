//! Go: `function_declaration`, `method_declaration` (qualified by its receiver type), `type_spec` (struct and
//! interface specialise the kind), `const_spec`, `import_spec`, `call_expression` callees. `Test*` functions
//! in `_test.go` files are tests. Interface satisfaction is structural in Go, so there are no implements edges.

use tree_sitter::Node;

use super::NodeKind;
use super::walker::{Ctx, Def, Item, cap};
use super::{METHOD, last_segment};

pub fn classify<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    match node.kind() {
        "function_declaration" => function(ctx, node, None),
        "method_declaration" => function(ctx, node, Some(receiver_type(ctx, node))),
        "type_spec" => type_spec(ctx, node),
        "const_spec" => def(ctx, node, NodeKind::Const, None, None),
        "import_spec" => Item::Imports(import(ctx, node)),
        "call_expression" => call(ctx, node),
        _ => Item::Other,
    }
}

fn function<'t>(ctx: &Ctx<'_>, node: Node<'t>, parent: Option<String>) -> Item<'t> {
    let kind = if parent.is_some() {
        NodeKind::Method
    } else {
        NodeKind::Function
    };
    def(ctx, node, kind, node.child_by_field_name("body"), parent)
}

fn def<'t>(
    ctx: &Ctx<'_>,
    node: Node<'t>,
    kind: NodeKind,
    body: Option<Node<'t>>,
    parent: Option<String>,
) -> Item<'t> {
    let name = ctx.field(node, "name");
    if name.is_empty() {
        return Item::Other;
    }
    let test = ctx.rel.ends_with("_test.go") && name.starts_with("Test");
    Item::Def(Def {
        kind,
        name: name.to_owned(),
        span: node,
        body,
        heritage: Vec::new(),
        parent,
        test,
    })
}

fn type_spec<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let kind = match node.child_by_field_name("type").map(|found| found.kind()) {
        Some("struct_type") => NodeKind::Struct,
        Some("interface_type") => NodeKind::Trait,
        _ => NodeKind::Type,
    };
    def(ctx, node, kind, None, None)
}

fn receiver_type(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    let text = ctx.field(node, "receiver");
    let word = text
        .trim_matches(['(', ')'])
        .split_whitespace()
        .last()
        .unwrap_or("");
    word.trim_start_matches('*')
        .split('[')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn import(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(String, String)> {
    let path = ctx.field(node, "path").trim_matches(['"', '`']);
    vec![(last_segment(path).to_owned(), path.to_owned())]
}

fn call<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(function) = node.child_by_field_name("function") else {
        return Item::Other;
    };
    let (dst, qual) = match function.kind() {
        "identifier" => (ctx.text(function).to_owned(), String::new()),
        "selector_expression" => (
            ctx.field(function, "field").to_owned(),
            operand(ctx, function),
        ),
        _ => return Item::Other,
    };
    Item::Call {
        dst,
        qual,
        opens_test: false,
    }
}

fn operand(ctx: &Ctx<'_>, selector: Node<'_>) -> String {
    let operand = selector.child_by_field_name("operand");
    match operand {
        Some(node) if node.kind() == "identifier" => ctx.text(node).to_owned(),
        _ => METHOD.to_owned(),
    }
}

pub fn doc(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    let anchor = node
        .parent()
        .filter(|up| matches!(up.kind(), "type_declaration" | "const_declaration"))
        .unwrap_or(node);
    let mut first = String::new();
    let mut expected = anchor.start_position().row;
    let mut previous = anchor.prev_sibling();
    while let Some(comment) = previous
        .filter(|found| found.kind() == "comment" && found.end_position().row + 1 == expected)
    {
        ctx.text(comment)
            .trim_start_matches("//")
            .trim()
            .clone_into(&mut first);
        expected = comment.start_position().row;
        previous = comment.prev_sibling();
    }
    cap(&first)
}
