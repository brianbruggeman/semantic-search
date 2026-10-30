//! The budgeted default text reply: sections are chosen by priority tier, not by hit order alone.
//!
//! Tier 1 is every hit's header, signature and doc; tier 2 its calls / called by / tests with `file:line`; tier 3 the
//! member names of container kinds; the shared `related:` line is last. A budget is `ceil(chars / 4)` tokens, so it
//! is held as `4 * budget` characters. Pure functions over the `ss-proto` payloads, like the rest of `render`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use ss_proto::{Capped, Facet, Findings, Member, Record};

use super::{FOOTER_CHARS, Slot, edge_line, more_suffix, priority_order, related_text, shown};

/// Hits at or past this rank (0-based) lose their members first when `demote_members` is set.
const MEMBER_RANKS: usize = 3;
const CONTAINERS: [&str; 4] = ["struct", "enum", "trait", "type"];

/// How the default reply is laid out and trimmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compact {
    /// Tokens (chars / 4) the whole reply may use, footer included.
    pub budget: usize,
    /// Members of hits below rank 3 are the very last section kept.
    pub demote_members: bool,
    /// Hits grouped by file: the path once, then `:start-end` per hit.
    pub grouped: bool,
    /// Header without container and score.
    pub lean: bool,
    /// Hits at or past this rank (0-based) print the header only, no signature or doc.
    pub sig_ranks: usize,
}

impl Compact {
    /// The shipped `--compact` preset: grouped paths, lean headers, signature and doc for the top hit only.
    #[must_use]
    pub fn lean(budget: usize) -> Self {
        Self {
            grouped: true,
            lean: true,
            sig_ranks: 1,
            ..Self::new(budget)
        }
    }

    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            demote_members: false,
            grouped: false,
            lean: false,
            sig_ranks: usize::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Head,
    Edges(u8),
    Members,
    Related,
}

struct Section {
    slot: Slot,
    tier: u8,
    part: Part,
    file: String,
    text: String,
    items: u32,
}

fn chars_of(text: &str) -> usize {
    text.chars().count()
}

fn head_text(record: &Record, here: &Path, opts: &Compact, rank: usize) -> String {
    let grouped = opts.grouped;
    let place = shown(&record.path, here);
    let mut text = String::new();
    let (kind, range, tail) = match &record.symbol {
        Some(symbol) => (
            format!("{} {}", symbol.kind, symbol.name),
            (symbol.start_line, symbol.end_line),
            tail_of(opts, &record.container, record.score),
        ),
        None => (
            "chunk".to_owned(),
            (record.start_line, record.end_line),
            tail_of(opts, "", record.score),
        ),
    };
    if grouped {
        let _ = writeln!(text, " {kind} :{}-{}{tail}", range.0, range.1);
    } else {
        let _ = writeln!(text, "{kind}  {place}:{}-{}{tail}", range.0, range.1);
    }
    if rank >= opts.sig_ranks {
        return text;
    }
    match &record.symbol {
        Some(symbol) => {
            let _ = writeln!(text, "  sig: {}", symbol.signature);
            if !symbol.doc.is_empty() {
                let _ = writeln!(text, "  doc: {}", symbol.doc);
            }
        }
        None => record.preview.iter().for_each(|line| {
            let _ = writeln!(text, "  | {}", line.trim_end());
        }),
    }
    text
}

fn tail_of(opts: &Compact, container: &str, score: f64) -> String {
    match (opts.lean, container.is_empty()) {
        (true, _) => String::new(),
        (false, true) => format!("  {score:.4}"),
        (false, false) => format!("  {container}  {score:.4}"),
    }
}

fn member_names(list: &Capped<Member>) -> Option<String> {
    if list.items.is_empty() {
        return None;
    }
    let names: Vec<&str> = list.items.iter().map(|m| m.name.as_str()).collect();
    Some(format!(
        "  members: {}{}\n",
        names.join(", "),
        more_suffix(list.more)
    ))
}

fn is_container(record: &Record) -> bool {
    record
        .symbol
        .as_ref()
        .is_some_and(|symbol| CONTAINERS.contains(&symbol.kind.as_str()))
}

fn count<Item>(list: &Capped<Item>) -> u32 {
    u32::try_from(list.items.len()).unwrap_or(u32::MAX) + list.more
}

fn sections_of(record: &Record, slot: Slot, here: &Path, opts: &Compact) -> Vec<Section> {
    let home = record.path.as_str();
    let mk = |tier: u8, part: Part, text: String, items: u32| Section {
        slot,
        tier,
        part,
        file: shown(&record.path, here),
        text,
        items,
    };
    let mut out = vec![mk(0, Part::Head, head_text(record, here, opts, slot.1), 0)];
    if record.symbol.is_none() {
        return out;
    }
    let edges = [
        ("calls", &record.calls),
        ("called by", &record.called_by),
        ("tests", &record.tests),
    ];
    for (index, (label, list)) in edges.iter().enumerate() {
        if let Some(text) = edge_line(label, list, home, here) {
            let part = Part::Edges(u8::try_from(index).unwrap_or(0));
            out.push(mk(1, part, text, count(list)));
        }
    }
    let late = opts.demote_members && slot.1 >= MEMBER_RANKS;
    if let Some(text) = is_container(record)
        .then(|| member_names(&record.members))
        .flatten()
    {
        out.push(mk(
            if late { 3 } else { 2 },
            Part::Members,
            text,
            count(&record.members),
        ));
    }
    out
}

fn build(findings: &Findings, here: &Path, opts: &Compact, order: &[Slot]) -> Vec<Section> {
    let position: HashMap<Slot, usize> = order.iter().copied().zip(0..).collect();
    let mut all: Vec<Section> = order
        .iter()
        .flat_map(|slot| {
            let record = &findings.facets[slot.0].records[slot.1];
            sections_of(record, *slot, here, opts)
        })
        .collect();
    let related = related_text(&findings.related, here);
    if !related.is_empty() {
        all.push(Section {
            slot: (usize::MAX, 0),
            tier: 4,
            part: Part::Related,
            file: String::new(),
            text: related,
            items: 0,
        });
    }
    all.sort_by_key(|section| {
        (
            section.tier,
            position.get(&section.slot).copied().unwrap_or(0),
        )
    });
    all
}

/// Indexes into `sections` that fit `limit` characters after `overhead`; the first head is always kept.
fn select(sections: &[Section], limit: usize, overhead: usize, grouped: bool) -> HashSet<usize> {
    let mut kept = HashSet::new();
    let (mut used, mut heads, mut files) = (overhead, HashSet::new(), HashSet::new());
    let mut closed = false;
    for (index, section) in sections.iter().enumerate() {
        let new_file = grouped && section.part == Part::Head && !files.contains(&section.file);
        let cost = chars_of(&section.text)
            + if new_file {
                chars_of(&section.file) + 1
            } else {
                0
            };
        let fits = used + cost <= limit || (section.part == Part::Head && heads.is_empty());
        let open = section.part != Part::Head || !closed;
        let owned = section.part == Part::Head
            || section.part == Part::Related
            || heads.contains(&section.slot);
        if !(fits && open && owned) {
            closed |= section.part == Part::Head;
            continue;
        }
        used += cost;
        kept.insert(index);
        if section.part == Part::Head {
            heads.insert(section.slot);
            files.insert(section.file.clone());
        }
    }
    kept
}

fn footer(
    sections: &[Section],
    kept: &HashSet<usize>,
    heads: &HashSet<Slot>,
    budget: usize,
) -> String {
    let (mut hits, mut links, mut members) = (0_u32, 0_u32, 0_u32);
    for (index, section) in sections.iter().enumerate() {
        if kept.contains(&index) {
            continue;
        }
        match section.part {
            Part::Head => hits += 1,
            Part::Edges(_) if heads.contains(&section.slot) => links += section.items,
            Part::Members if heads.contains(&section.slot) => members += section.items,
            _ => {}
        }
    }
    let parts: Vec<String> = [(hits, "hits"), (links, "links"), (members, "members")]
        .into_iter()
        .filter(|(number, _)| *number > 0)
        .map(|(number, label)| format!("{number} {label}"))
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    format!("cut: {} (use --budget {})\n", parts.join(", "), budget * 2)
}

fn by_slot<'a>(sections: &'a [Section], kept: &HashSet<usize>) -> HashMap<Slot, Vec<&'a Section>> {
    let mut grouped: HashMap<Slot, Vec<&Section>> = HashMap::new();
    for (index, section) in sections.iter().enumerate() {
        if kept.contains(&index) && section.part != Part::Related {
            grouped.entry(section.slot).or_default().push(section);
        }
    }
    for list in grouped.values_mut() {
        list.sort_by_key(|section| match section.part {
            Part::Head => 0,
            Part::Edges(which) => 1 + which,
            _ => 9,
        });
    }
    grouped
}

fn facet_text(
    facet: &Facet,
    index: usize,
    parts: &HashMap<Slot, Vec<&Section>>,
    grouped: bool,
) -> String {
    let ranks: Vec<usize> = (0..facet.records.len())
        .filter(|rank| parts.contains_key(&(index, *rank)))
        .collect();
    let mut out = String::new();
    let mut files: Vec<&str> = Vec::new();
    for rank in &ranks {
        let file = parts[&(index, *rank)][0].file.as_str();
        if !files.contains(&file) {
            files.push(file);
        }
    }
    if !grouped {
        files = vec![""];
    }
    for file in files {
        if grouped {
            let _ = writeln!(out, "{file}");
        }
        for rank in ranks
            .iter()
            .filter(|rank| !grouped || parts[&(index, **rank)][0].file == file)
        {
            parts[&(index, *rank)]
                .iter()
                .for_each(|section| out.push_str(&section.text));
        }
    }
    out
}

/// The budgeted reply: facets in query order, hits in rank order (or grouped by file), then `related:` and a footer
/// naming what the budget cut and how to ask for more. Never exceeds `opts.budget` tokens except for the top hit.
#[must_use]
pub fn compact_text(findings: &Findings, here: &Path, opts: &Compact) -> String {
    let order = priority_order(&findings.facets);
    let sections = build(findings, here, opts, &order);
    let multi = findings.facets.len() > 1;
    let overhead: usize = if multi {
        findings.facets.iter().map(|f| chars_of(&f.query) + 3).sum()
    } else {
        0
    };
    let limit = opts.budget * 4;
    let mut kept = select(&sections, limit, overhead, opts.grouped);
    let mut note = String::new();
    if kept.len() < sections.len() {
        kept = select(
            &sections,
            limit.saturating_sub(FOOTER_CHARS),
            overhead,
            opts.grouped,
        );
        let heads: HashSet<Slot> = sections
            .iter()
            .enumerate()
            .filter(|(index, s)| s.part == Part::Head && kept.contains(index))
            .map(|(_, s)| s.slot)
            .collect();
        note = footer(&sections, &kept, &heads, opts.budget);
    }
    let parts = by_slot(&sections, &kept);
    let mut out = String::new();
    for (index, facet) in findings.facets.iter().enumerate() {
        if multi {
            let _ = writeln!(out, "# {}", facet.query);
        }
        out.push_str(&facet_text(facet, index, &parts, opts.grouped));
    }
    sections
        .iter()
        .enumerate()
        .filter(|(index, s)| s.part == Part::Related && kept.contains(index))
        .for_each(|(_, s)| out.push_str(&s.text));
    out.push_str(&note);
    out
}
