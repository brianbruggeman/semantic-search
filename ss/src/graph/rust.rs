//! Rust: `function_item`, `struct_item`/`enum_item`/`trait_item`/`type_item`, `mod_item`, `const_item`/`static_item`,
//! `impl_item` (type + optional trait), `use_declaration` leaves, `call_expression` callee names.

use tree_sitter::Node;

use super::METHOD;
use super::NodeKind;
use super::walker::{Ctx, Def, Item, cap};

pub fn classify<'t>(ctx: &Ctx<'_>, node: Node<'t>, in_type: bool) -> Item<'t> {
    match node.kind() {
        "function_item" | "function_signature_item" => define(ctx, node, NodeKind::Function),
        "struct_item" | "union_item" => define(ctx, node, NodeKind::Struct),
        "enum_item" => define(ctx, node, NodeKind::Enum),
        "trait_item" => define(ctx, node, NodeKind::Trait),
        "type_item" if !in_type => define(ctx, node, NodeKind::Type),
        "mod_item" => define(ctx, node, NodeKind::Module),
        "const_item" | "static_item" => define(ctx, node, NodeKind::Const),
        "impl_item" => implementation(ctx, node),
        "use_declaration" => imports(ctx, node),
        "call_expression" => call(ctx, node),
        _ => Item::Other,
    }
}

fn define<'t>(ctx: &Ctx<'_>, node: Node<'t>, kind: NodeKind) -> Item<'t> {
    let name = ctx.field(node, "name");
    if name.is_empty() {
        return Item::Other;
    }
    Item::Def(Def {
        kind,
        name: name.to_owned(),
        span: node,
        body: node.child_by_field_name("body"),
        heritage: Vec::new(),
        parent: None,
        test: kind == NodeKind::Function && is_test(ctx, node),
    })
}

fn is_test(ctx: &Ctx<'_>, node: Node<'_>) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" if ctx.text(sibling).contains("test") => return true,
            "attribute_item" | "line_comment" | "block_comment" => {
                previous = sibling.prev_sibling();
            }
            _ => return false,
        }
    }
    false
}

fn implementation<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(target) = node.child_by_field_name("type") else {
        return Item::Other;
    };
    Item::Impl {
        ty: type_name(ctx, target),
        tr: node
            .child_by_field_name("trait")
            .map(|found| type_name(ctx, found)),
        body: node.child_by_field_name("body"),
    }
}

fn type_name(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    match node.kind() {
        "generic_type" => node
            .child_by_field_name("type")
            .map_or_else(String::new, |inner| type_name(ctx, inner)),
        "scoped_type_identifier" => ctx.field(node, "name").to_owned(),
        _ => ctx.text(node).to_owned(),
    }
}

fn imports<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let mut leaves = Vec::new();
    if let Some(argument) = node.child_by_field_name("argument") {
        use_leaves(ctx, argument, "", &mut leaves);
    }
    Item::Imports(leaves)
}

fn join(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_owned()
    } else if tail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}::{tail}")
    }
}

fn use_leaves(ctx: &Ctx<'_>, node: Node<'_>, prefix: &str, out: &mut Vec<(String, String)>) {
    match node.kind() {
        "identifier" | "crate" | "self" | "super" => {
            out.push((ctx.text(node).to_owned(), prefix.to_owned()));
        }
        "scoped_identifier" => out.push((
            ctx.field(node, "name").to_owned(),
            join(prefix, ctx.field(node, "path")),
        )),
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                use_leaves(ctx, path, prefix, out);
            }
        }
        "scoped_use_list" => {
            let inner = join(prefix, ctx.field(node, "path"));
            if let Some(list) = node.child_by_field_name("list") {
                use_leaves(ctx, list, &inner, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                use_leaves(ctx, child, prefix, out);
            }
        }
        "use_wildcard" => {
            let path = ctx.text(node).trim_end_matches("::*");
            out.push(("*".to_owned(), join(prefix, path)));
        }
        _ => {}
    }
}

fn call<'t>(ctx: &Ctx<'_>, node: Node<'t>) -> Item<'t> {
    let Some(function) = node.child_by_field_name("function") else {
        return Item::Other;
    };
    callee(ctx, function).map_or(Item::Other, |(dst, qual)| Item::Call {
        dst,
        qual,
        opens_test: false,
    })
}

fn callee(ctx: &Ctx<'_>, function: Node<'_>) -> Option<(String, String)> {
    match function.kind() {
        "identifier" => Some((ctx.text(function).to_owned(), String::new())),
        "scoped_identifier" => Some((
            ctx.field(function, "name").to_owned(),
            ctx.field(function, "path").to_owned(),
        )),
        "field_expression" => {
            let receiver = ctx.field(function, "value");
            let qual = if receiver == "self" { "self" } else { METHOD };
            Some((ctx.field(function, "field").to_owned(), qual.to_owned()))
        }
        "generic_function" => function
            .child_by_field_name("function")
            .and_then(|inner| callee(ctx, inner)),
        _ => None,
    }
}

pub fn doc(ctx: &Ctx<'_>, node: Node<'_>) -> String {
    let mut first = String::new();
    let mut previous = node.prev_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" => {
                let text = ctx.text(sibling);
                if let Some(line) = text
                    .strip_prefix("///")
                    .or_else(|| text.strip_prefix("//!"))
                {
                    line.trim().clone_into(&mut first);
                }
            }
            "attribute_item" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_sibling();
    }
    cap(&first)
}
