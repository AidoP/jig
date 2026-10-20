//! Files generated from template strings by `file` statements, which are written when the project is configured.

use std::path::PathBuf;

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Ref, Referent};
use super::{Project, normalize};

/// A file declared by a `file` statement, whose contents are known once the configuration is read, so it is written
/// then, rather than by ninja, which could not write contents with a newline.
pub struct GeneratedFile {
    pub name: String,
    pub name_span: Span,
    /// Where it is written, relative to the project root, which is `<build-dir>/<name>` unless `output` replaces it.
    pub output: PathBuf,
    /// The contents of the `content` attribute, as its template evaluated to.
    pub content: String,
    /// The permissions of the file in the build directory, from `mode`.
    pub mode: u32,
    /// Where `jig install` installs the file, if it does, which is installed with `mode` unless `install-mode` replaces
    /// it.
    pub install: Install,
}
impl GeneratedFile {
    /// The text which is written, which ends with a newline, as the last line of a multi-line string does not, unless
    /// it is empty.
    pub fn text(&self) -> String {
        let mut text = self.content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }
}

/// The permissions of a file without `mode`, which are those of data.
const DEFAULT_MODE: u32 = 0o644;

impl Project {
    /// Take a `file` statement, whose attributes are `content`, the template string of its contents, `output`, a path
    /// relative to the build directory, as the outputs of a custom command are, `mode`, its permissions, and the install
    /// attributes.
    pub(super) fn parse_file_statement(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { text: name, span, .. }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        let build_dir = self.build_dir();
        let mut output = normalize(&build_dir.join(&name));
        let mut content = None;
        let mut mode = None;
        let mut install = Install::data();
        let mut install_mode = false;
        // an attribute which failed has been reported, so `content` is not also reported as missing
        let reported = self.diagnostics.len();
        while let Some(attribute) = self.next_attribute(parser, &[]) {
            match attribute.name {
                "output" => {
                    if let Some(path) = self.operand_path(attribute.operand(), &build_dir) {
                        output = path;
                    }
                },
                "content" => {
                    if let Some(text) = self.str_operand(attribute.operand()) {
                        content = Some(text);
                    }
                },
                "mode" => {
                    if let Some(value) = self.mode_operand(attribute.operand()) {
                        mode = Some(value);
                    }
                },
                name @ ("install" | "install-dir" | "install-mode") => {
                    install_mode |= name == "install-mode";
                    self.install_attribute(&mut install, name, attribute.operand());
                },
                name => self.diagnostics.push(Diagnostic {
                    span: Some(attribute.name_span),
                    message: Message::UnknownAttribute { keyword: Item::File.keyword(), name: name.into() },
                    severity: Severity::Error,
                }),
            }
        }
        let Some(content) = content else {
            if self.diagnostics.len() == reported {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::MissingAttribute("content"),
                    severity: Severity::Error,
                });
            }
            return;
        };
        if let Some(&previous) = self.generated_idx.get(&name) {
            let previous = self.generated[previous].name_span;
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::RedeclareCustom { item: Item::File, name, previous },
                severity: Severity::Error,
            });
            return;
        }
        // the file is referred to by its name, as a custom command is, such as by an input or a `link-script`
        let reference = Ref { referent: Referent::File(name.clone()), paths: vec![output.clone()] };
        self.declare_reference(name.clone(), span, Item::File, reference);
        self.generated_idx.insert(name.clone(), self.generated.len());
        // a file is installed with its own permissions by default, such as a script which is executable
        let mode = mode.unwrap_or(DEFAULT_MODE);
        if !install_mode {
            install.mode = mode;
        }
        self.generated.push(GeneratedFile { name, name_span: span, output, content, mode, install });
    }
    /// Returns the file declared by a `file` statement with a name.
    pub(super) fn generated_file(&self, name: &str) -> Option<&GeneratedFile> {
        self.generated_idx.get(name).map(|&index| &self.generated[index])
    }
}
