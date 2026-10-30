//! `graph_eval arms`: the default-output arms of `ai_docs/design.md` (rule fixed before running).
// harness: aborts on any setup failure
#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;

use serde_json::json;
use ss::context::{follow_ups, test_refs};
use ss::query::{search, search_findings};
use ss::render::{Compact, compact_text, findings_text, lines_text, tokens};
use ss::store::Store;
use ss_proto::Findings;

use super::{
    Gold, SUFFICIENT, load_gold, open_index, present_anywhere, present_with_site, share, summary,
};

struct Arm {
    name: String,
    render: fn(&Findings, &Path, &ArmSpec, usize) -> String,
    spec: ArmSpec,
}

#[derive(Clone, Copy)]
struct ArmSpec {
    flat: Option<usize>,
    per_hit: Option<usize>,
    demote: bool,
    grouped: bool,
    lean: bool,
    sig_ranks: usize,
}

fn render_full(findings: &Findings, root: &Path, _: &ArmSpec, _: usize) -> String {
    findings_text(findings, root, None)
}

fn render_compact(findings: &Findings, root: &Path, spec: &ArmSpec, limit: usize) -> String {
    let budget = spec
        .flat
        .or_else(|| spec.per_hit.map(|each| each * limit))
        .unwrap_or(400);
    let opts = Compact {
        demote_members: spec.demote,
        grouped: spec.grouped,
        lean: spec.lean,
        sig_ranks: spec.sig_ranks,
        ..Compact::new(budget)
    };
    compact_text(findings, root, &opts)
}

fn arm_grid() -> Vec<Arm> {
    let none = ArmSpec {
        flat: None,
        per_hit: None,
        demote: false,
        grouped: false,
        lean: false,
        sig_ranks: usize::MAX,
    };
    let mut arms = vec![Arm {
        name: "A0 full".into(),
        render: render_full,
        spec: none,
    }];
    let budgets: [(&str, Option<usize>, Option<usize>); 6] = [
        ("200", Some(200), None),
        ("250", Some(250), None),
        ("300", Some(300), None),
        ("400", Some(400), None),
        ("40n", None, Some(40)),
        ("45n", None, Some(45)),
    ];
    let shapes = [
        ("a", false, false),
        ("a+b", true, false),
        ("a+c", false, true),
        ("a+b+c", true, true),
    ];
    for (label, flat, per_hit) in budgets {
        for (suffix, demote, grouped) in shapes {
            arms.push(Arm {
                name: format!("{label} {suffix}"),
                render: render_compact,
                spec: ArmSpec {
                    flat,
                    per_hit,
                    demote,
                    grouped,
                    ..none
                },
            });
        }
    }
    arms.extend(post_hoc_arms(none));
    arms
}

fn post_hoc_arms(none: ArmSpec) -> Vec<Arm> {
    let mut arms = Vec::new();
    for budget in [200, 220, 222, 230, 240, 250, 260, 300] {
        for sig_ranks in [1, 2, 3, usize::MAX] {
            for grouped in [false, true] {
                let depth = if sig_ranks == usize::MAX {
                    "all".into()
                } else {
                    sig_ranks.to_string()
                };
                arms.push(Arm {
                    name: format!(
                        "PH {budget} lean sig{depth}{}",
                        if grouped { " c" } else { "" }
                    ),
                    render: render_compact,
                    spec: ArmSpec {
                        flat: Some(budget),
                        lean: true,
                        sig_ranks,
                        grouped,
                        ..none
                    },
                });
            }
        }
    }
    arms
}

struct Targets {
    dev: bool,
    core: Vec<String>,
    siblings: Vec<String>,
}

fn last_segment(name: &str) -> String {
    name.rsplit("::").next().unwrap_or(name).to_owned()
}

fn targets_for(store: &Store, root: &Path, gold: &Gold) -> Option<Targets> {
    let node = store
        .primary_node(&gold.path, gold.start, gold.end)
        .expect("primary")?;
    let (outgoing, incoming, siblings) = follow_ups(store, root, &node).expect("follow ups");
    let tests = test_refs(store, root, &node).expect("tests");
    let mut core: Vec<String> = outgoing.iter().map(|e| last_segment(&e.name)).collect();
    core.extend(incoming.iter().map(|e| last_segment(&e.name)));
    core.extend(tests.iter().map(|e| last_segment(&e.name)));
    let unique: HashSet<String> = core.into_iter().collect();
    let sibling_names: HashSet<String> = siblings.iter().map(|m| m.name.clone()).collect();
    Some(Targets {
        dev: gold.start % 2 == 1,
        core: unique.into_iter().collect(),
        siblings: sibling_names.into_iter().collect(),
    })
}

#[derive(Default)]
struct ArmTally {
    tokens: [Vec<usize>; 2],
    suff: [usize; 2],
    split: [(usize, usize); 2],
    denominator: usize,
    sib_any: (usize, usize),
    sib_line: usize,
    core_line: (usize, usize),
    hits_cut: usize,
}

fn cut_hits(text: &str) -> usize {
    text.lines()
        .find_map(|line| line.strip_prefix("cut: "))
        .and_then(|rest| rest.split(',').next())
        .and_then(|part| part.trim().split_once(' '))
        .filter(|(_, label)| label.starts_with("hits"))
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0)
}

pub async fn run_arms(out: &Path) {
    let work = tempfile::tempdir().expect("work dir");
    let (shard, root, config, _pool) = open_index(work.path()).await;
    let store = Store::open(&config).expect("store");
    let golds = load_gold();
    assert_eq!(golds.len(), 44, "N queries");
    let arms = arm_grid();
    let mut tallies: Vec<ArmTally> = arms.iter().map(|_| ArmTally::default()).collect();
    let mut old_tokens: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    for gold in &golds {
        let targets = targets_for(&store, &root, gold);
        let queries = vec![gold.query.clone()];
        for (slot, limit) in [5_usize, 10].into_iter().enumerate() {
            let hits = search(&shard, &root, &gold.query, limit)
                .await
                .expect("search");
            old_tokens[slot].push(tokens(&lines_text(&hits, &root)));
            let findings = search_findings(&shard, &root, &queries, limit)
                .await
                .expect("findings");
            for (arm, tally) in arms.iter().zip(tallies.iter_mut()) {
                let text = (arm.render)(&findings, &root, &arm.spec, limit);
                tally.tokens[slot].push(tokens(&text));
                if slot == 0 {
                    tally.hits_cut += cut_hits(&text);
                }
                score(tally, slot, &text, targets.as_ref());
            }
        }
    }
    arms_report(&arms, &tallies, &old_tokens);
    let dump: Vec<_> = arms
        .iter()
        .zip(&tallies)
        .map(|(arm, t)| {
            json!({"arm": arm.name, "n5": t.tokens[0], "n10": t.tokens[1], "suff": t.suff,
            "denominator": t.denominator})
        })
        .collect();
    std::fs::write(out, serde_json::to_string_pretty(&dump).expect("json")).expect("write");
}

fn score(tally: &mut ArmTally, slot: usize, text: &str, targets: Option<&Targets>) {
    let Some(targets) = targets.filter(|t| !t.core.is_empty()) else {
        return;
    };
    let found = targets
        .core
        .iter()
        .filter(|name| present_with_site(text, name))
        .count();
    let pass = share(found, targets.core.len()) >= SUFFICIENT;
    if pass {
        tally.suff[slot] += 1;
    }
    if slot == 0 {
        let half = usize::from(targets.dev);
        tally.split[half].0 += 1;
        tally.split[half].1 += usize::from(pass);
        tally.denominator += 1;
        tally.core_line.0 += targets.core.len();
        tally.core_line.1 += found;
        tally.sib_any.0 += targets.siblings.len();
        tally.sib_any.1 += targets
            .siblings
            .iter()
            .filter(|n| present_anywhere(text, n))
            .count();
        tally.sib_line += targets
            .siblings
            .iter()
            .filter(|n| present_with_site(text, n))
            .count();
    }
}

fn arms_report(arms: &[Arm], tallies: &[ArmTally], old: &[Vec<usize>; 2]) {
    let (old5, old10) = (summary(&old[0]), summary(&old[1]));
    println!(
        "old --lines: n5 {old5:?} n10 {old10:?}; limits n5 median<=222 p95<=342, n10 median<=449 p95<=632"
    );
    println!(
        "arm | n5 med/p95 | n10 med/p95 | rule1 | suff n5 (of denom) | suff n10 | core targets present | siblings any/total | siblings with line | hits cut n5 | held-out suff / dev suff"
    );
    let base = tallies[0].suff[0];
    for (arm, t) in arms.iter().zip(tallies) {
        assert_eq!(t.tokens[0].len(), 44, "N per arm");
        let (at5, at10) = (summary(&t.tokens[0]), summary(&t.tokens[1]));
        let rule1 = at5.0 <= 222 && at10.0 <= 449 && at5.1 <= 342 && at10.1 <= 632;
        println!(
            "{:<10} | {}/{} | {}/{} | {} | {} of {} (rule2 {}) | {} | {} of {} | {} of {} | {} | {} | {}/{} of {}/{}",
            arm.name,
            at5.0,
            at5.1,
            at10.0,
            at10.1,
            if rule1 { "meets" } else { "fails" },
            t.suff[0],
            t.denominator,
            if t.suff[0] >= base { "meets" } else { "fails" },
            t.suff[1],
            t.core_line.1,
            t.core_line.0,
            t.sib_any.1,
            t.sib_any.0,
            t.sib_line,
            t.hits_cut,
            t.split[0].1,
            t.split[1].1,
            t.split[0].0,
            t.split[1].0
        );
    }
}
