//! Tests of the book, in `docs/src`, which the tests run from the project root find: that what it lists is what jig
//! has, and that its links lead somewhere. That its examples configure is an integration test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The book's pages, as their paths and text.
fn pages() -> Vec<(PathBuf, String)> {
    let mut dirs = vec![PathBuf::from("docs/src")];
    let mut pages = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                let text = std::fs::read_to_string(&path).unwrap();
                pages.push((path, text));
            }
        }
    }
    pages.sort();
    pages
}

/// Returns a page's text, by its path within the book.
fn page(path: &str) -> String {
    std::fs::read_to_string(Path::new("docs/src").join(path)).unwrap()
}

/// Returns the lines of a page which aren't in a code block.
fn prose(text: &str) -> impl Iterator<Item = &str> {
    let mut in_code = false;
    text.lines().filter(move |line| {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            return false;
        }
        !in_code
    })
}

/// Returns the id mdBook gives a heading, which a link to it names after `#`: its text in lower case, with spaces as
/// `-`, and only letters, digits, `-` and `_`.
fn heading_id(heading: &str) -> String {
    let heading = heading.to_lowercase().replace(' ', "-");
    heading.chars().filter(|&c| c.is_alphanumeric() || c == '-' || c == '_').collect()
}

#[test]
fn lists_every_diagnostic() {
    // the diagnostics page has a row for each diagnostic's identifier, by which its severity is overridden
    let text = page("language/diagnostics.md");
    let rows = text.lines().filter_map(|line| line.strip_prefix("| `")?.split_once("` |").map(|(id, _)| id));
    let listed: BTreeSet<&str> = rows.collect();
    let ids: BTreeSet<&str> = crate::project::DIAGNOSTIC_IDS.iter().copied().collect();
    assert_eq!(ids.difference(&listed).collect::<Vec<_>>(), Vec::<&&str>::new(), "diagnostics the book doesn't list");
    assert_eq!(listed.difference(&ids).collect::<Vec<_>>(), Vec::<&&str>::new(), "diagnostics jig doesn't have");
}

#[test]
fn lists_every_builtin_property() {
    // the builtin properties page has an item for each, by its name, or a pattern of names, as `codegen/<option>`, of
    // those which are declared as they are first used, or for each option, lint or diagnostic
    let text = page("language/builtin-properties.md");
    let items = prose(&text).filter_map(|line| line.strip_prefix("- ")?.split_once(':').map(|(names, _)| names));
    let mut listed = BTreeSet::new();
    for names in items {
        // `cache/dir`, `cache/size` and `cache/remote`
        for name in names.split('`').skip(1).step_by(2) {
            listed.insert(name.to_string());
        }
    }
    let patterns: Vec<String> = listed.iter().filter_map(|name| Some(name[..name.find('<')?].to_string())).collect();
    let mut project = crate::project::Project::new(PathBuf::from("."), Vec::new());
    crate::declare_builtins(&mut project, "/sysroot", "x86_64-unknown-linux-gnu", "1.97.0".into(), &[], None);
    let declared: BTreeSet<&str> = project.declared().into_iter().collect();
    let unlisted: Vec<_> = declared
        .iter()
        .filter(|name| !listed.contains(**name) && !patterns.iter().any(|pattern| name.starts_with(pattern.as_str())))
        .collect();
    assert_eq!(unlisted, Vec::<&&str>::new(), "builtin properties the book doesn't list");
    let missing: Vec<_> = listed.iter().filter(|name| !name.contains('<') && !declared.contains(name.as_str())).collect();
    assert_eq!(missing, Vec::<&String>::new(), "builtin properties jig doesn't have");
}

#[test]
fn links_lead_somewhere() {
    // each link between the book's pages leads to a page, and one to a heading leads to a heading of it, as mdBook
    // names it, while every page is in its summary, as mdBook makes a page the summary lists which is missing
    let pages = pages();
    let headings = |text: &str| -> BTreeSet<String> {
        prose(text).filter_map(|line| Some(heading_id(line.strip_prefix('#')?.trim_start_matches('#').trim()))).collect()
    };
    let mut broken = Vec::new();
    for (path, text) in &pages {
        for line in prose(text) {
            let mut rest = line;
            while let Some(at) = rest.find("](") {
                rest = &rest[at + 2..];
                let Some(end) = rest.find(')') else { break };
                let link = &rest[..end];
                rest = &rest[end..];
                if link.contains("://") {
                    continue;
                }
                let (file, heading) = link.split_once('#').map_or((link, None), |(file, heading)| (file, Some(heading)));
                let target = if file.is_empty() { path.clone() } else { path.parent().unwrap().join(file) };
                let Some((_, target_text)) = pages.iter().find(|(page, _)| normalize(page) == normalize(&target)) else {
                    broken.push(format!("{}: {link}, which is no page", path.display()));
                    continue;
                };
                if let Some(heading) = heading && !headings(target_text).contains(heading) {
                    broken.push(format!("{}: {link}, which is no heading", path.display()));
                }
            }
        }
    }
    let summary = page("SUMMARY.md");
    for (path, _) in &pages {
        let name = path.strip_prefix("docs/src").unwrap().to_string_lossy().replace('\\', "/");
        if name != "SUMMARY.md" && !summary.contains(&format!("]({name})")) {
            broken.push(format!("{name}, which the summary doesn't list"));
        }
    }
    assert_eq!(broken, Vec::<String>::new());
}

/// Returns a path with its `.` and `..` resolved, without the file system, to compare pages by.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {},
            std::path::Component::ParentDir => {
                normal.pop();
            },
            component => normal.push(component),
        }
    }
    normal
}

#[test]
fn finds_headings_as_mdbook_names_them() {
    assert_eq!(heading_id("Batched stat, and why there is no io_uring"), "batched-stat-and-why-there-is-no-io_uring");
    assert_eq!(heading_id("jig check and rust-analyzer"), "jig-check-and-rust-analyzer");
}
