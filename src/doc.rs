//! `jig doc`, which documents crates with rustdoc and says where their documentation is.

use crate::Error;
use crate::configured::{Configured, Output, Request};
use crate::rig_builder::Options;

/// Document the crates with `doc`, or only those named, then show the first page of each, relative to the working
/// directory, and with `open`, open the first of them in the browser.
pub fn doc(names: &[String], open: bool, build: Options) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    // the configuration was brought up to date, so the database is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let mut docs = Vec::new();
    for name in names {
        match database.docs.iter().find(|doc| doc.name == name) {
            Some(doc) => docs.push(doc),
            None => return Err(Error::UnknownDoc(name.clone())),
        }
    }
    if names.is_empty() {
        docs = database.docs.iter().collect();
    }
    if docs.is_empty() {
        return Err(Error::NoDocs);
    }
    let targets: Vec<String> = if names.is_empty() {
        vec!["doc".to_string()]
    } else {
        names.iter().map(|name| format!("doc/{name}")).collect()
    };
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    project.build(&Request { targets: &targets, build, ..Default::default() })?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let pages: Vec<std::path::PathBuf> = docs.iter().map(|doc| project.root.join(doc.index)).collect();
    for (doc, page) in docs.iter().zip(&pages) {
        let shown = page.strip_prefix(&cwd).unwrap_or(page);
        println!("{}: {}", doc.name, shown.display());
    }
    if open {
        crate::platform::open(&pages[0])?;
    }
    Ok(())
}
