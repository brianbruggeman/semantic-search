//! Python: `function_definition`, `class_definition` (with `superclasses`), module-level `UPPER_CASE` `assignment`,
//! `import_statement`/`import_from_statement`, `call` callee names. Tests are `test_*` functions.

use tree_sitter::Node;

use super::walker::{Ctx, Def, Item, cap};
use super::{EdgeKind, NodeKind, last_segment};

pub fn classify<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    match node.kind() {
        "function_definition" => define(ctx, node, NodeKind::Function, Vec::new()),
        "class_definition" => define(ctx, node, NodeKind::Type, bases(ctx, node)),
        "assignment" => constant(ctx, node),
        "import_statement" => Item::Imports(plain_imports(ctx, node)),
        "import_from_statement" => Item::Imports(from_imports(ctx, node)),
        "call" => call(ctx, node),
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
    Item::Def(Def {
        kind,
        name: name.to_owned(),
        span: node,
        body: node.child_by_field_name("body"),
        heritage,
        parent: None,
        test: kind == NodeKind::Function && name.starts_with("test"),
    })
}

fn bases(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(EdgeKind, String)> {
    let Some(list) = node.child_by_field_name("superclasses") else {
        return Vec::new();
    };
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "identifier" | "attribute"))
        .map(|child| (EdgeKind::Extends, last_segment(ctx.text(child)).to_owned()))
        .collect()
}

fn constant<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(left) = node.child_by_field_name("left") else {
        return Item::Other;
    };
    let name = ctx.text(left);
    let upper = name
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
    if left.kind() != "identifier"
        || !upper
        || node
            .parent()
            .is_none_or(|up| up.kind() != "expression_statement")
    {
        return Item::Other;
    }
    Item::Def(Def {
        kind: NodeKind::Const,
        name: name.to_owned(),
        span: node,
        body: None,
        heritage: Vec::new(),
        parent: None,
        test: false,
    })
}

fn split_dotted(text: &str) -> (String, String) {
    match text.rsplit_once('.') {
        Some((qual, name)) => (name.to_owned(), qual.to_owned()),
        None => (text.to_owned(), String::new()),
    }
}

fn imported_name(ctx: &Ctx<'_>, node: Node<'_>) -> Option<String> {
    match node.kind() {
        "dotted_name" => Some(ctx.text(node).to_owned()),
        "aliased_import" => node
            .child_by_field_name("name")
            .map(|inner| ctx.text(inner).to_owned()),
        _ => None,
    }
}

fn plain_imports(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(String, String)> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|child| imported_name(ctx, child))
        .map(|text| split_dotted(&text))
        .collect()
}

fn from_imports(ctx: &Ctx<'_>, node: Node<'_>) -> Vec<(String, String)> {
    let module = ctx.field(node, "module_name").to_owned();
    let mut cursor = node.walk();
    let names: Vec<(String, String)> = node
        .children_by_field_name("name", &mut cursor)
        .filter_map(|child| imported_name(ctx, child))
        .map(|name| (name, module.clone()))
        .collect();
    let mut walker = node.walk();
    let wildcard = node
        .named_children(&mut walker)
        .any(|child| child.kind() == "wildcard_import");
    if wildcard {
        return vec![("*".to_owned(), module)];
    }
    names
}

fn call<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(function) = node.child_by_field_name("function") else {
        return Item::Other;
    };
    let (dst, qual) = match function.kind() {
        "identifier" => (ctx.text(function).to_owned(), String::new()),
        "attribute" => (
            ctx.field(function, "attribute").to_owned(),
            receiver(ctx, function),
        ),
        _ => return Item::Other,
    };
    Item::Call {
        dst,
        qual,
        opens_test: false,
    }
}

fn receiver(ctx: &Ctx<'_>, attribute: Node<'_>) -> String {
    let object = attribute.child_by_field_name("object");
    match object {
        Some(node) if node.kind() == "identifier" => match ctx.text(node) {
            "self" | "cls" => "self".to_owned(),
            other => other.to_owned(),
        },
        _ => String::new(),
    }
}

pub fn doc(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    let first = node
        .child_by_field_name("body")
        .and_then(|body| body.named_child(0))
        .filter(|statement| statement.kind() == "expression_statement")
        .and_then(|statement| statement.named_child(0))
        .filter(|string| string.kind() == "string");
    let Some(string) = first else {
        return String::new();
    };
    let text = ctx.text(string).trim_matches(['"', '\'']);
    cap(text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(""))
}
