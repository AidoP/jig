//! Generation of `.rust-project.json`, which describes the crates to rust-analyzer.

use std::path::Path;

use crate::generate::Unit;

/// Write the project file.
///
/// The flycheck for a crate runs `jig check` with its label. Paths are made absolute, as rust-analyzer resolves
/// them relative to the project file, which may be opened from elsewhere.
pub fn write(w: &mut dyn std::io::Write, sysroot: &Path, configure: &Path, units: &[Unit]) -> std::io::Result<()> {
    let sysroot_src = sysroot.join("lib/rustlib/src/rust/library");
    let root = std::env::current_dir().unwrap_or_default();
    writeln!(w, "{{")?;
    writeln!(w, "    \"sysroot_src\": {},", path(&sysroot_src))?;
    writeln!(w, "    \"runnables\": [")?;
    writeln!(w, "        {{")?;
    writeln!(w, "            \"kind\": \"flycheck\",")?;
    writeln!(w, "            \"program\": {},", path(configure))?;
    writeln!(w, "            \"args\": [\"check\", \"{{label}}\"],")?;
    writeln!(w, "            \"cwd\": {}", path(&root))?;
    writeln!(w, "        }}")?;
    writeln!(w, "    ],")?;
    writeln!(w, "    \"crates\": [")?;
    let entries: Vec<_> = units.iter().map(|unit| entry(unit, units)).collect();
    writeln!(w, "{}", entries.join(",\n"))?;
    writeln!(w, "    ]")?;
    writeln!(w, "}}")
}

fn entry(unit: &Unit, units: &[Unit]) -> String {
    let deps: Vec<_> = unit.deps.iter()
        .map(|&index| format!("{{ \"crate\": {index}, \"name\": {} }}", string(&units[index].name)))
        .collect();
    let cfg = if unit.test { "\"test\"" } else { "" };
    let kind = match unit.kind {
        crate::project::rust::Kind::Bin => "bin",
        crate::project::rust::Kind::Lib | crate::project::rust::Kind::ProcMacro => "lib",
    };
    let mut entry = String::from("        {\n");
    entry.push_str(&format!("            \"display_name\": {},\n", string(&unit.name)));
    entry.push_str(&format!("            \"root_module\": {},\n", path(&unit.root)));
    entry.push_str("            \"edition\": \"2024\",\n");
    // a custom target is given to rustc as its specification
    let target = match &unit.spec {
        Some(spec) => path(spec),
        None => string(&unit.target),
    };
    entry.push_str(&format!("            \"target\": {target},\n"));
    entry.push_str(&format!("            \"is_workspace_member\": {},\n", unit.member));
    // rust-analyzer loads procedural macros itself to expand them
    if unit.kind == crate::project::rust::Kind::ProcMacro {
        entry.push_str("            \"is_proc_macro\": true,\n");
        entry.push_str(&format!("            \"proc_macro_dylib_path\": {},\n", path(&unit.output)));
    }
    entry.push_str(&format!("            \"cfg\": [{cfg}],\n"));
    entry.push_str(&format!(
        "            \"build\": {{ \"label\": {}, \"build_file\": {}, \"target_kind\": \"{kind}\" }},\n",
        string(&unit.label),
        path(&unit.build_file),
    ));
    entry.push_str(&format!("            \"deps\": [{}]\n", deps.join(", ")));
    entry.push_str("        }");
    entry
}

pub fn absolute(path: &Path) -> std::path::PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Returns a path, made absolute, as a JSON string literal.
fn path(path: &Path) -> String {
    string(&absolute(path).display().to_string())
}

/// Returns a string as a JSON string literal.
pub fn string(s: &str) -> String {
    let mut json = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            '\n' => json.push_str("\\n"),
            c if (c as u32) < 0x20 => json.push_str(&format!("\\u{:04x}", c as u32)),
            c => json.push(c),
        }
    }
    json.push('"');
    json
}
