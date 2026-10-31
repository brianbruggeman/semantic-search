//! Heading trees of prose files: Markdown ATX headings and reStructuredText underlined titles.

use super::Entry;

const RST_ADORNMENT: &str = "=-~^\"`#*+:.'_";
const GIST_CHARS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DocKind {
    Markdown,
    Rst,
}

pub(super) fn doc_kind(path: &str) -> Option<DocKind> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "md" | "mdx" | "markdown" => Some(DocKind::Markdown),
        "rst" | "txt" => Some(DocKind::Rst),
        _ => None,
    }
}

struct Heading {
    level: usize,
    title: String,
    first: usize,
    last: usize,
}

pub(super) fn entries(kind: DocKind, text: &str) -> Vec<Entry> {
    let lines: Vec<&str> = text.lines().collect();
    let headings = match kind {
        DocKind::Markdown => atx_headings(&lines),
        DocKind::Rst => rst_headings(&lines),
    };
    sections(kind, &lines, &headings)
}

fn frontmatter_end(lines: &[&str]) -> usize {
    if lines.first().copied() != Some("---") {
        return 0;
    }
    lines
        .iter()
        .skip(1)
        .position(|line| matches!(*line, "---" | "..."))
        .map_or(0, |close| close + 2)
}

fn atx_headings(lines: &[&str]) -> Vec<Heading> {
    let mut found = Vec::new();
    let mut fenced = false;
    for (index, line) in lines.iter().enumerate().skip(frontmatter_end(lines)) {
        let text = line.trim_start();
        if text.starts_with("```") || text.starts_with("~~~") {
            fenced = !fenced;
        } else if let (false, Some((level, title))) = (fenced, atx(line)) {
            found.push(Heading { level, title, first: index, last: index });
        }
    }
    found
}

fn atx(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start().len();
    let text = line.trim_start();
    let level = text.chars().take_while(|letter| *letter == '#').count();
    let rest = &text[level..];
    let spaced = rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t');
    if indent > 3 || !(1..=6).contains(&level) || !spaced {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    Some((level, title.to_owned()))
}

fn adornment(line: &str) -> Option<char> {
    let first = line.chars().next()?;
    let uniform = line.chars().all(|letter| letter == first);
    (uniform && line.chars().count() >= 3 && RST_ADORNMENT.contains(first)).then_some(first)
}

fn rst_headings(lines: &[&str]) -> Vec<Heading> {
    let mut styles: Vec<char> = Vec::new();
    let mut found = Vec::new();
    for index in 1..lines.len() {
        let title = lines[index - 1];
        let underline = adornment(lines[index].trim_end());
        let titled = !title.trim().is_empty() && !title.starts_with(char::is_whitespace);
        let long_enough = lines[index].trim_end().chars().count() >= title.trim().chars().count();
        let Some(style) = underline.filter(|_| titled && long_enough && adornment(title).is_none())
        else {
            continue;
        };
        let level = styles.iter().position(|seen| *seen == style).unwrap_or_else(|| {
            styles.push(style);
            styles.len() - 1
        }) + 1;
        found.push(Heading { level, title: title.trim().to_owned(), first: index - 1, last: index });
    }
    found
}

fn sections(kind: DocKind, lines: &[&str], headings: &[Heading]) -> Vec<Entry> {
    let mut open: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    for (position, heading) in headings.iter().enumerate() {
        while open.last().is_some_and(|level| *level >= heading.level) {
            open.pop();
        }
        let next = headings.get(position + 1).map_or(lines.len(), |next| next.first);
        let close = headings[position + 1..]
            .iter()
            .find(|later| later.level <= heading.level)
            .map_or(lines.len(), |later| later.first);
        let end = last_filled(lines, heading.last, close);
        let mark = match kind {
            DocKind::Markdown => "#".repeat(heading.level),
            DocKind::Rst => format!("{}.", heading.level),
        };
        out.push(Entry {
            depth: open.len(),
            range: (heading.first + 1, end + 1),
            label: format!("{mark} {}", heading.title),
            name: heading.title.clone(),
            gist: gist(lines, heading.last + 1, next),
            implementation: false,
        });
        open.push(heading.level);
    }
    out
}

fn last_filled(lines: &[&str], floor: usize, close: usize) -> usize {
    (floor..close)
        .rev()
        .find(|index| !lines[*index].trim().is_empty())
        .unwrap_or(floor)
}

fn gist(lines: &[&str], from: usize, to: usize) -> String {
    lines[from.min(to)..to]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(GIST_CHARS).collect())
        .unwrap_or_default()
}
