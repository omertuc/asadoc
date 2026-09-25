//! `asadoc check`: what's resolved, ignored, and still to resolve.

use crate::eval::Evaluation;
use crate::repo::MarkedCode;

fn describe(code: &MarkedCode) -> String {
    match &code.section {
        Some(s) => format!("section \"{s}\" of {}", code.file),
        None => code.file.clone(),
    }
}

fn label(file: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{file}, section {s}"),
        None => file.to_string(),
    }
}

/// The whole picture; false when anything needs attention
pub fn check_all(ev: &Evaluation) -> bool {
    let mut open = 0;
    for a in &ev.assemblies {
        let todo: Vec<_> = a.blocks.iter().filter(|b| !b.done()).collect();
        let ignored = a.blocks.iter().filter(|b| b.ignored_as.is_some()).count();
        println!("{}: {} resolved, {ignored} ignored, {} to resolve", a.assembly.id, a.blocks.len() - todo.len() - ignored, todo.len());
        for b in &todo {
            let hint = match b.candidates.first() {
                None => String::new(),
                Some(c) if c.plan.is_some() => format!("  → {} (fixable)", label(&c.file, c.name.as_deref())),
                Some(c) => format!("  → {} ({}% similar)", label(&c.file, c.name.as_deref()), (c.similarity * 100.0).round()),
            };
            println!("  {}{hint}", b.block.reference);
        }
        open += todo.len();
    }
    for i in &ev.unused {
        println!("marked code no doc block matches: {}", describe(&ev.scan.marked[*i]));
    }
    for (reason, content) in &ev.stale_ignored {
        println!("stale ignore entry ({reason}): {}", content.lines().next().unwrap_or(""));
    }
    for p in &ev.scan.problems {
        println!("marker problem: {}: {}", p.file, p.message);
    }
    open == 0 && ev.scan.problems.is_empty()
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
            println!("✓ {r}: resolved (matches {})", codes.join(", "));
            for (p, v) in &b.matches[0].values {
                println!("    {p} = {v:?}");
            }
            continue;
        }
        ok = false;
        let Some(top) = b.candidates.first() else {
            println!("✗ {r}: no marked code matches it, and none resembles it");
            continue;
        };
        let unmarked = matches!(top.kind, "unmarked-file" | "lines");
        println!(
            "✗ {r}: no marked code matches it; the most similar is {}{}",
            label(&top.file, top.name.as_deref()),
            if unmarked { " (not marked)" } else { "" }
        );
        let doc_label = format!("doc: {r}{}", if top.doc_options.is_empty() { "" } else { " (doc options applied)" });
        let repo_label = format!("{}{}", label(&top.file, top.name.as_deref()), if top.options.is_empty() { "" } else { ", marker options applied" });
        let diff = similar::TextDiff::from_lines(&top.doc, &top.content);
        print!("{}", diff.unified_diff().context_radius(3).header(&doc_label, &repo_label));
    }
    ok
}
