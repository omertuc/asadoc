//! `asadoc check`: what's resolved, ignored, and still to resolve.

use crate::eval::{CandidateInfo, Evaluation};
use crate::repo::MarkedCode;

fn describe(code: &MarkedCode) -> String {
    match &code.section {
        Some(s) => format!("{}, section \"{s}\"", code.file),
        None => code.file.clone(),
    }
}

fn label(file: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{file}, section {s}"),
        None => file.to_string(),
    }
}

fn describe_candidate(c: &CandidateInfo) -> String {
    let what = match c.kind {
        "unmarked-file" => format!("{} (not marked)", c.file),
        "lines" => format!("{}, lines {}-{} (not marked)", c.file, c.lines.map_or(0, |l| l.0), c.lines.map_or(0, |l| l.1)),
        _ => match &c.name {
            Some(s) => format!("{}, section \"{s}\"", c.file),
            None => c.file.clone(),
        },
    };
    if c.plan.is_some() {
        format!("{what} — a one-click fix in `asadoc serve` makes it match")
    } else {
        format!("{what} ({}% similar)", (c.similarity * 100.0).round())
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The whole picture; false when anything needs attention
pub fn check_all(ev: &Evaluation) -> bool {
    let total: usize = ev.assemblies.iter().map(|a| a.blocks.len()).sum();
    let resolved = ev.blocks().filter(|(_, b)| b.resolved()).count();
    let ignored = ev.blocks().filter(|(_, b)| b.ignored_as.is_some()).count();
    let open = total - resolved - ignored;
    let problems = &ev.scan.problems;

    println!(
        "{} checked: {resolved} match marked code in this repo, {ignored} are ignored, {open} match nothing.",
        plural(total, "doc code block", "doc code blocks")
    );

    if open > 0 {
        println!("\nDoc code blocks that no marked code matches ({open}):");
        println!("Make repo code match each one (or ignore it if it doesn't come from this repo).");
        for a in &ev.assemblies {
            let todo: Vec<_> = a.blocks.iter().filter(|b| !b.done()).collect();
            if todo.is_empty() {
                continue;
            }
            println!("\n  {}", a.assembly.title);
            for b in todo {
                println!("    ✗ {}", b.block.reference);
                println!("        in the docs:  modules/{}.adoc, line {}", b.block.module, b.block.line);
                match b.candidates.first() {
                    Some(c) => println!("        closest code: {}", describe_candidate(c)),
                    None => println!("        closest code: nothing in the repo resembles it"),
                }
            }
        }
    }

    if !ev.unused.is_empty() {
        println!("\nMarked code that no doc block matches ({}):", ev.unused.len());
        println!("Code marked as appearing in the docs, but the docs no longer show it as is.");
        for i in &ev.unused {
            let code = &ev.scan.marked[*i];
            println!("    ! {}", describe(code));
            // The unresolved block this code resembles most (any of a block's candidates)
            let closest = ev
                .blocks()
                .filter_map(|(_, b)| b.candidates.iter().find(|c| c.id == code.id).map(|c| (b, c.similarity)))
                .max_by(|x, y| x.1.total_cmp(&y.1));
            match closest {
                Some((b, sim)) => println!("        closest doc block: {} ({}% similar, listed above)", b.block.reference, (sim * 100.0).round()),
                None => println!("        closest doc block: none resembles it; if the docs dropped it, remove its markers"),
            }
        }
    }

    if !ev.stale_ignored.is_empty() {
        println!("\nIgnored entries no doc block has anymore ({}):", ev.stale_ignored.len());
        println!("Delete their files from the ignore directory (or remove them in `asadoc serve`).");
        for (reason, content) in &ev.stale_ignored {
            println!("    - {reason}: {}", content.lines().next().unwrap_or(""));
        }
    }

    if !problems.is_empty() {
        println!("\nMarkers that can't be read ({}):", problems.len());
        for p in problems {
            println!("    ! {}: {}", p.file, p.message);
        }
    }

    let ok = open == 0 && problems.is_empty();
    if ok {
        println!("\n✓ Every doc code block matches marked code or is ignored.");
    } else {
        println!("\nTo see how a block differs from its closest code: asadoc check <block>");
        println!("To work through them with one-click fixes:        asadoc serve");
        println!("How markers work:                                 asadoc guide");
    }
    ok
}

/// Specific blocks: resolved, ignored, or why not with a diff; false unless all are done
pub fn check_blocks(ev: &Evaluation, refs: &[String]) -> bool {
    let mut ok = true;
    for r in refs {
        let Some((_, b)) = ev.blocks().find(|(_, b)| &b.block.reference == r) else {
            println!("✗ {r}: no such doc block in the configured assemblies");
            ok = false;
            continue;
        };
        if let Some(reason) = &b.ignored_as {
            println!("✓ {r}: ignored ({reason})");
            continue;
        }
        if b.resolved() {
            let codes: Vec<String> = b.matches.iter().map(|m| describe(&ev.scan.marked[m.code])).collect();
            println!("✓ {r}: resolved (matches {})", codes.join("; "));
            for (p, v) in &b.matches[0].values {
                println!("    {p} = {v:?}");
            }
            continue;
        }
        ok = false;
        let Some(top) = b.candidates.first() else {
            println!("✗ {r}: no marked code matches it, and nothing in the repo resembles it");
            continue;
        };
        println!("✗ {r}: no marked code matches it; the closest is {}", describe_candidate(top));
        let doc_label = format!("doc: {r}{}", if top.doc_options.is_empty() { "" } else { " (doc options applied)" });
        let repo_label = format!("{}{}", label(&top.file, top.name.as_deref()), if top.options.is_empty() { "" } else { ", marker options applied" });
        let diff = similar::TextDiff::from_lines(&top.doc, &top.content);
        print!("{}", diff.unified_diff().context_radius(3).header(&doc_label, &repo_label));
    }
    ok
}
