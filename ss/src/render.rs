//! Text and JSON renderings of the structured replies, and the token budget that trims them.
//!
//! Pure functions over the `ss-proto` payloads: no pipe, no state. A token is `ceil(chars / 4)` of the exact bytes
//! written to stdout, so the budget a caller sets is the same quantity the measurements report.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use ss_proto::{
    Capped, EdgeRef, Facet, Findings, GraphReply, Hit, Member, Neighborhood, Record, Related,
    Resolved,
};

mod compact;
mod exact;
mod plain;

pub use compact::{Compact, compact_text};
pub use exact::exact_text;
pub use plain::{Layout, Mode, exact_reply, plain_text};

/// Tokens held back for the "cut" footer whenever a budget may drop hits.
/// Characters held back for a `cut:` footer once a first pass already cut something.
const FOOTER_CHARS: usize = 72;

/// Default `--compact` budget in tokens.
pub const COMPACT_BUDGET: usize = 250;
/// Default budget in tokens of an exact search reply.
pub const EXACT_BUDGET: usize = 300;

pub const FOOTER_RESERVE: usize = 14;

#[must_use]
pub fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// One query per `-q`, plus the positional words: if any positional word holds a space each word is its own query,
/// otherwise they are one query joined by spaces (so `ss walk files` stays a single query).
#[must_use]
pub fn facets(positional: &[String], flagged: &[String]) -> Vec<String> {
    let mut queries: Vec<String> = flagged.to_vec();
    let spaced = positional
        .iter()
        .any(|word| word.contains(char::is_whitespace));
    if spaced {
        queries.extend(positional.iter().cloned());
    } else if !positional.is_empty() {
        queries.push(positional.join(" "));
    }
    queries
}

fn shown(path: &str, here: &Path) -> String {
    Path::new(path)
        .strip_prefix(here)
        .map_or_else(|_| path.to_owned(), |rel| rel.display().to_string())
}

fn site(entry_path: &str, line: u32, home: &str, here: &Path) -> String {
    if entry_path == home {
        format!(":{line}")
    } else {
        format!("{}:{line}", shown(entry_path, here))
    }
}

fn edge_text(entry: &EdgeRef, home: &str, here: &Path) -> String {
    let mark = if entry.resolved == Resolved::Ambiguous {
        "?"
    } else {
        ""
    };
    format!(
        "{}{mark} ({})",
        entry.name,
        site(&entry.path, entry.line, home, here)
    )
}

fn more_suffix(list_more: u32) -> String {
    if list_more > 0 {
        format!(", +{list_more} more")
    } else {
        String::new()
    }
}

fn edge_line(label: &str, list: &Capped<EdgeRef>, home: &str, here: &Path) -> Option<String> {
    if list.items.is_empty() {
        return None;
    }
    let body: Vec<String> = list
        .items
        .iter()
        .map(|entry| edge_text(entry, home, here))
        .collect();
    Some(format!(
        "  {label}: {}{}\n",
        body.join(", "),
        more_suffix(list.more)
    ))
}

fn member_line(list: &Capped<Member>, home: &str, here: &Path) -> Option<String> {
    if list.items.is_empty() {
        return None;
    }
    let body: Vec<String> = list
        .items
        .iter()
        .map(|member| {
            format!(
                "{} {}",
                member.name,
                site(&member.path, member.line, home, here)
            )
        })
        .collect();
    Some(format!(
        "  members: {}{}\n",
        body.join(", "),
        more_suffix(list.more)
    ))
}

/// One hit as a compact record: header, signature, doc, then the non-empty neighbour lines.
#[must_use]
pub fn record_text(record: &Record, here: &Path) -> String {
    let place = shown(&record.path, here);
    let Some(symbol) = &record.symbol else {
        let mut text = format!(
            "chunk  {place}:{}-{}  {:.4}\n",
            record.start_line, record.end_line, record.score
        );
        record.preview.iter().for_each(|line| {
            let _ = writeln!(text, "  | {}", line.trim_end());
        });
        return text;
    };
    let mut text = format!(
        "{} {}  {place}:{}-{}  {}  {:.4}\n",
        symbol.kind,
        symbol.name,
        symbol.start_line,
        symbol.end_line,
        record.container,
        record.score
    );
    let _ = writeln!(text, "  sig: {}", symbol.signature);
    if !symbol.doc.is_empty() {
        let _ = writeln!(text, "  doc: {}", symbol.doc);
    }
    let home = record.path.as_str();
    let lines = [
        edge_line("calls", &record.calls, home, here),
        edge_line("called by", &record.called_by, home, here),
        edge_line("tests", &record.tests, home, here),
        member_line(&record.members, home, here),
    ];
    lines
        .into_iter()
        .flatten()
        .for_each(|line| text.push_str(&line));
    text
}

fn related_text(related: &Capped<Related>, here: &Path) -> String {
    if related.items.is_empty() {
        return String::new();
    }
    let body: Vec<String> = related
        .items
        .iter()
        .map(|item| {
            format!(
                "{} ({}:{}) x{}",
                item.name,
                shown(&item.path, here),
                item.line,
                item.mentions
            )
        })
        .collect();
    format!(
        "related: {}{}\n",
        body.join(", "),
        more_suffix(related.more)
    )
}

type Slot = (usize, usize);

/// Records in budget priority: every facet's rank 1, then every facet's rank 2, and so on.
fn priority_order(facets: &[Facet]) -> Vec<Slot> {
    let depth = facets
        .iter()
        .map(|facet| facet.records.len())
        .max()
        .unwrap_or(0);
    (0..depth)
        .flat_map(|rank| (0..facets.len()).map(move |facet| (facet, rank)))
        .filter(|(facet, rank)| *rank < facets[*facet].records.len())
        .collect()
}

/// Greedy prefix of `order` that fits `budget` after `overhead`; the first slot is always kept.
fn keep_within(
    order: &[Slot],
    cost: impl Fn(Slot) -> usize,
    budget: Option<usize>,
    overhead: usize,
) -> HashSet<Slot> {
    let Some(limit) = budget else {
        return order.iter().copied().collect();
    };
    let mut kept = HashSet::new();
    let mut used = overhead;
    for (index, slot) in order.iter().enumerate() {
        let reserve = if index + 1 < order.len() {
            FOOTER_RESERVE
        } else {
            0
        };
        if !kept.is_empty() && used + cost(*slot) + reserve > limit {
            break;
        }
        used += cost(*slot);
        kept.insert(*slot);
    }
    kept
}

/// The whole text reply: facets in query order, records in rank order, the shared `related:` line, and a footer
/// naming how many hits the budget cut.
#[must_use]
pub fn findings_text(findings: &Findings, here: &Path, budget: Option<usize>) -> String {
    let multi = findings.facets.len() > 1;
    let texts: Vec<Vec<String>> = findings
        .facets
        .iter()
        .map(|facet| {
            facet
                .records
                .iter()
                .map(|record| record_text(record, here))
                .collect()
        })
        .collect();
    let order = priority_order(&findings.facets);
    let headers: usize = if multi {
        findings
            .facets
            .iter()
            .map(|f| tokens(&format!("# {}\n", f.query)))
            .sum()
    } else {
        0
    };
    let kept = keep_within(
        &order,
        |(facet, rank)| tokens(&texts[facet][rank]),
        budget,
        headers,
    );
    let mut out = String::new();
    for (index, facet) in findings.facets.iter().enumerate() {
        if multi {
            let _ = writeln!(out, "# {}", facet.query);
        }
        (0..facet.records.len())
            .filter(|rank| kept.contains(&(index, *rank)))
            .for_each(|rank| out.push_str(&texts[index][rank]));
    }
    let related = related_text(&findings.related, here);
    if budget.is_none_or(|limit| tokens(&out) + tokens(&related) + FOOTER_RESERVE <= limit) {
        out.push_str(&related);
    }
    let cut = order.len() - kept.len();
    if cut > 0 {
        let _ = writeln!(out, "(+{cut} hits cut by --budget {})", budget.unwrap_or(0));
    }
    out
}

/// The JSON reply: `schema`, `facets`, `related`, plus `cut` (hits dropped by the budget) and `budget` when set.
#[must_use]
pub fn findings_json(findings: &Findings, budget: Option<usize>) -> String {
    let cost = |(facet, rank): Slot| {
        serde_json::to_string(&findings.facets[facet].records[rank]).map_or(0, |text| tokens(&text))
    };
    let order = priority_order(&findings.facets);
    let kept = keep_within(&order, cost, budget, 40);
    let facets: Vec<Value> = findings
        .facets
        .iter()
        .enumerate()
        .map(|(index, facet)| {
            let records: Vec<&Record> = facet
                .records
                .iter()
                .enumerate()
                .filter(|(rank, _)| kept.contains(&(index, *rank)))
                .map(|(_, record)| record)
                .collect();
            json!({"query": facet.query, "records": records})
        })
        .collect();
    let mut body =
        json!({"schema": findings.schema, "facets": facets, "cut": order.len() - kept.len()});
    let related = serde_json::to_string(&findings.related).unwrap_or_default();
    if budget.is_none_or(|limit| tokens(&body.to_string()) + tokens(&related) <= limit) {
        body["related"] = json!(findings.related);
    }
    if let Some(limit) = budget {
        body["budget"] = json!(limit);
    }
    body.to_string()
}

fn neighbor_lines(near: &Neighborhood, here: &Path) -> String {
    let home = near.symbol.path.as_str();
    let mut text = String::new();
    let direct = |list: &Capped<EdgeRef>, depth: u8| Capped {
        items: list
            .items
            .iter()
            .filter(|entry| entry.depth == depth)
            .cloned()
            .collect(),
        more: if depth == 1 { list.more } else { 0 },
        hidden: 0,
    };
    let sections = [
        ("callers", direct(&near.callers, 1)),
        ("callers2", direct(&near.callers, 2)),
        ("callees", direct(&near.callees, 1)),
        ("callees2", direct(&near.callees, 2)),
        ("importers", near.importers.clone()),
        ("tests", near.tests.clone()),
    ];
    for (label, list) in &sections {
        text.extend(edge_line(label, list, home, here));
    }
    text.extend(member_line(&near.members, home, here));
    text
}

/// `ss graph` text: per symbol a header, signature, doc and the neighbour lines.
#[must_use]
pub fn graph_text(reply: &GraphReply, here: &Path) -> String {
    let mut out = String::new();
    for near in &reply.neighborhoods.items {
        let symbol = &near.symbol;
        let _ = writeln!(
            out,
            "{} {}  {}:{}-{}",
            symbol.kind,
            symbol.qpath,
            shown(&symbol.path, here),
            symbol.start_line,
            symbol.end_line
        );
        let _ = writeln!(out, "  sig: {}", symbol.signature);
        if !symbol.doc.is_empty() {
            let _ = writeln!(out, "  doc: {}", symbol.doc);
        }
        out.push_str(&neighbor_lines(near, here));
    }
    if reply.neighborhoods.more > 0 {
        let _ = writeln!(
            out,
            "(+{} more symbols match; use path:line)",
            reply.neighborhoods.more
        );
    }
    out
}

#[must_use]
pub fn graph_json(reply: &GraphReply) -> String {
    serde_json::to_string(reply).unwrap_or_default()
}

/// The pre-graph format: `path:start-end  score` and the first lines of the chunk.
#[must_use]
pub fn lines_text(hits: &[Hit], here: &Path) -> String {
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(
            out,
            "{}:{}-{}  {:.4}",
            shown(&hit.path, here),
            hit.start_line,
            hit.end_line,
            hit.score
        );
        hit.preview.iter().for_each(|line| {
            let _ = writeln!(out, "    {line}");
        });
    }
    out
}

#[cfg(test)]
mod tests;
