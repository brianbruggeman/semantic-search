//! Shells out to `git`: the only I/O in the history view. Everything after the call is pure text.

use std::path::Path;
use std::process::Command;

use super::HistoryError;

/// What a git command should print: per-file line counts, or a patch with this many context lines.
#[derive(Debug, Clone, Copy)]
pub enum Dump {
    Stat,
    Patch(u32),
}

/// Where the compared texts come from: the history view reads both sides of a change from here.
#[derive(Debug, Clone)]
pub enum Source {
    Show(String),
    Diff { revs: Vec<String>, staged: bool },
}

#[derive(Debug, Clone)]
pub enum Side {
    Rev(String),
    Index,
    Work,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub sign: char,
    pub number: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<PatchLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

pub fn run(dir: &Path, args: &[String]) -> Result<String, HistoryError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HistoryError::Git(stderr.trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl Source {
    #[must_use]
    pub fn command(&self, dump: Dump, path: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = match self {
            Self::Show(_) => vec!["show".into()],
            Self::Diff { .. } => vec!["diff".into()],
        };
        args.push("--no-renames".into());
        args.extend(Self::dump_flags(dump, matches!(self, Self::Show(_))));
        match self {
            Self::Show(rev) => args.push(rev.clone()),
            Self::Diff { revs, staged } => {
                if *staged {
                    args.push("--staged".into());
                }
                args.extend(revs.iter().cloned());
            }
        }
        if let Some(path) = path {
            args.extend(["--".into(), path.to_owned()]);
        }
        args
    }

    fn dump_flags(dump: Dump, show: bool) -> Vec<String> {
        match (dump, show) {
            (Dump::Stat, true) => vec!["--numstat".into(), "--format=@@%h %s".into()],
            (Dump::Stat, false) => vec!["--numstat".into()],
            (Dump::Patch(context), true) => vec!["--format=".into(), format!("-U{context}")],
            (Dump::Patch(context), false) => vec![format!("-U{context}")],
        }
    }

    #[must_use]
    pub fn sides(&self) -> (Side, Side) {
        match self {
            Self::Show(rev) => (Side::Rev(format!("{rev}^")), Side::Rev(rev.clone())),
            Self::Diff { revs, staged } => match (revs.as_slice(), staged) {
                ([], false) => (Side::Index, Side::Work),
                ([], true) => (Side::Rev("HEAD".into()), Side::Index),
                ([first], false) => (Side::Rev(first.clone()), Side::Work),
                ([first], true) => (Side::Rev(first.clone()), Side::Index),
                ([first, second, ..], _) => (Side::Rev(first.clone()), Side::Rev(second.clone())),
            },
        }
    }
}

/// The text of `path` on one side, or `None` when the file does not exist there (added, deleted, root commit).
#[must_use]
pub fn content(dir: &Path, side: &Side, path: &str) -> Option<String> {
    match side {
        Side::Rev(rev) => run(dir, &["show".into(), format!("{rev}:{path}")]).ok(),
        Side::Index => run(dir, &["show".into(), format!(":{path}")]).ok(),
        Side::Work => {
            let top = run(dir, &["rev-parse".into(), "--show-toplevel".into()]).ok()?;
            std::fs::read_to_string(Path::new(top.trim()).join(path)).ok()
        }
    }
}

/// Parses `--numstat` output (plus the optional `@@<hash> <subject>` header line of `git show`).
#[must_use]
pub fn parse_stats(text: &str) -> (Option<String>, Vec<Stat>) {
    let header = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("@@"))
        .map(str::to_owned);
    let stats = text.lines().filter_map(parse_stat_line).collect();
    (header, stats)
}

fn parse_stat_line(line: &str) -> Option<Stat> {
    let mut fields = line.splitn(3, '\t');
    let (added, deleted, path) = (fields.next()?, fields.next()?, fields.next()?);
    Some(Stat {
        path: path.to_owned(),
        added: added.parse().unwrap_or(0),
        deleted: deleted.parse().unwrap_or(0),
        binary: added == "-",
    })
}

/// Parses a unified patch into per-file hunks, numbering each `+` line in the new file and each `-` line in the old.
#[must_use]
pub fn parse_patch(text: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut counters = (0, 0);
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, path)| path);
            files.push(FilePatch {
                path: path.to_owned(),
                hunks: Vec::new(),
            });
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            counters = hunk_starts(line);
            in_hunk = true;
            if let Some(file) = files.last_mut() {
                file.hunks.push(Hunk {
                    header: line.to_owned(),
                    lines: Vec::new(),
                });
            }
        } else if in_hunk {
            push_line(&mut files, &mut counters, line);
        }
    }
    files
}

fn hunk_starts(header: &str) -> (u32, u32) {
    let mut tokens = header.split_whitespace().skip(1);
    let mut next = || {
        tokens
            .next()
            .map(|token| token.trim_start_matches(['-', '+']))
            .and_then(|token| token.split(',').next())
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0)
    };
    let old = next();
    (old, next())
}

fn push_line(files: &mut [FilePatch], counters: &mut (u32, u32), line: &str) {
    let (old, new) = counters;
    let Some(hunk) = files.last_mut().and_then(|file| file.hunks.last_mut()) else {
        return;
    };
    let sign = line.chars().next().unwrap_or(' ');
    let text = line.get(1..).unwrap_or("").to_owned();
    let number = match sign {
        '-' => *old,
        '+' | ' ' => *new,
        _ => return,
    };
    if sign != '+' {
        *old += 1;
    }
    if sign != '-' {
        *new += 1;
    }
    hunk.lines.push(PatchLine { sign, number, text });
}
