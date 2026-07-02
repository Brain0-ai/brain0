//! Tree-sitter symbol extraction and structural AST fingerprinting for brain0.
//!
//! Given a repo-relative path and source text, [`parse_source`] returns a [`ParsedFile`]:
//! the extracted symbols (functions/methods/classes) with deterministic qualified paths
//! and [`Fingerprint`]s, plus a file-level fingerprint. When no grammar is available the
//! parser falls back to file-level granularity.
//!
//! Full symbol-level support is provided for **Python** and **TypeScript/JavaScript**;
//! new grammars can be added in `extract.rs` without touching the data model.

mod extract;
pub mod fingerprint;

pub use fingerprint::Fingerprint;

use brain0_model::Lang;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tree_sitter::Parser;

/// Errors produced while parsing.
#[derive(Debug, Error)]
pub enum ParserError {
    #[error("failed to load grammar: {0}")]
    Grammar(String),
    #[error("parser produced no tree (parse was cancelled or timed out)")]
    NoTree,
}

/// The kind of a syntactic symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
}

/// A single extracted symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    pub kind: SymbolKind,
    /// The symbol's own name (last path component).
    pub name: String,
    /// Deterministic hierarchical path: `"<rel_path>::<Outer>.<inner>"`.
    pub qualified_path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based inclusive line range.
    pub start_line: usize,
    pub end_line: usize,
    pub fingerprint: Fingerprint,
}

/// The result of parsing one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedFile {
    pub rel_path: String,
    /// `None` when no grammar matched (file-level fallback).
    pub lang: Option<Lang>,
    pub symbols: Vec<ExtractedSymbol>,
    /// Fingerprint of the whole file (structural when a grammar matched, content-based in
    /// fallback), used for file-level artifact identity and move detection.
    pub file_fingerprint: Fingerprint,
}

impl ParsedFile {
    /// True when extraction could not go below file granularity (no grammar available).
    #[must_use]
    pub fn is_file_fallback(&self) -> bool {
        self.lang.is_none()
    }
}

/// Parse `source` (logically located at `rel_path`) into a [`ParsedFile`].
///
/// The `rel_path` is used both to select the grammar (by extension) and to prefix the
/// qualified paths of extracted symbols. It should be normalized and repo-relative.
pub fn parse_source(rel_path: &str, source: &str) -> Result<ParsedFile, ParserError> {
    match extract::detect_language(rel_path) {
        Some(sel) => {
            let mut parser = Parser::new();
            parser
                .set_language(&sel.language)
                .map_err(|err| ParserError::Grammar(err.to_string()))?;
            let tree = parser.parse(source, None).ok_or(ParserError::NoTree)?;
            let root = tree.root_node();
            let symbols =
                extract::Extractor::new(rel_path, source.as_bytes(), sel.family).run(root);
            Ok(ParsedFile {
                rel_path: rel_path.to_owned(),
                lang: Some(Lang::new(sel.lang_name)),
                symbols,
                file_fingerprint: Fingerprint::of_node(root),
            })
        }
        None => Ok(ParsedFile {
            rel_path: rel_path.to_owned(),
            lang: None,
            symbols: Vec::new(),
            file_fingerprint: Fingerprint::of_text(source),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(parsed: &ParsedFile) -> Vec<&str> {
        parsed
            .symbols
            .iter()
            .map(|symbol| symbol.qualified_path.as_str())
            .collect()
    }

    fn find<'a>(parsed: &'a ParsedFile, qpath: &str) -> &'a ExtractedSymbol {
        parsed
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_path == qpath)
            .unwrap_or_else(|| panic!("symbol {qpath} not found; have {:?}", paths(parsed)))
    }

    #[test]
    fn python_extracts_functions_classes_and_methods() {
        let src = r#"
def top_level():
    return 1

class Greeter:
    def __init__(self, name):
        self.name = name

    def greet(self):
        return self.name
"#;
        let parsed = parse_source("pkg/mod.py", src).unwrap();
        assert_eq!(parsed.lang.as_ref().unwrap().as_str(), "python");
        let qpaths = paths(&parsed);
        assert!(qpaths.contains(&"pkg/mod.py::top_level"));
        assert!(qpaths.contains(&"pkg/mod.py::Greeter"));
        assert!(qpaths.contains(&"pkg/mod.py::Greeter.__init__"));
        assert!(qpaths.contains(&"pkg/mod.py::Greeter.greet"));

        assert_eq!(
            find(&parsed, "pkg/mod.py::top_level").kind,
            SymbolKind::Function
        );
        assert_eq!(find(&parsed, "pkg/mod.py::Greeter").kind, SymbolKind::Class);
        assert_eq!(
            find(&parsed, "pkg/mod.py::Greeter.greet").kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn rust_extracts_fns_types_impl_methods_and_scopes_mods() {
        let src = r#"
pub fn top_level() -> u32 { 1 }

pub struct Engine { n: u32 }

impl Engine {
    pub fn new() -> Self { Self { n: 0 } }
    fn bump(&mut self) { self.n += 1; }
}

pub trait Runner {
    fn run(&self) -> u32 { 0 }
}

pub enum Mode { A, B }

mod tests {
    fn helper() -> u32 { 2 }
}
"#;
        let parsed = parse_source("src/engine.rs", src).unwrap();
        assert_eq!(parsed.lang.as_ref().unwrap().as_str(), "rust");
        let qpaths = paths(&parsed);
        assert!(
            qpaths.contains(&"src/engine.rs::top_level"),
            "have {qpaths:?}"
        );
        assert!(qpaths.contains(&"src/engine.rs::Engine"), "have {qpaths:?}");
        assert!(
            qpaths.contains(&"src/engine.rs::Engine.new"),
            "have {qpaths:?}"
        );
        assert!(
            qpaths.contains(&"src/engine.rs::Engine.bump"),
            "have {qpaths:?}"
        );
        assert!(qpaths.contains(&"src/engine.rs::Runner"), "have {qpaths:?}");
        assert!(
            qpaths.contains(&"src/engine.rs::Runner.run"),
            "have {qpaths:?}"
        );
        assert!(qpaths.contains(&"src/engine.rs::Mode"), "have {qpaths:?}");
        // The mod scopes without emitting: helper is namespaced, no `::tests` symbol exists.
        assert!(
            qpaths.contains(&"src/engine.rs::tests.helper"),
            "have {qpaths:?}"
        );
        assert!(!qpaths.contains(&"src/engine.rs::tests"), "have {qpaths:?}");

        assert_eq!(
            find(&parsed, "src/engine.rs::top_level").kind,
            SymbolKind::Function
        );
        assert_eq!(
            find(&parsed, "src/engine.rs::Engine").kind,
            SymbolKind::Class
        );
        assert_eq!(
            find(&parsed, "src/engine.rs::Engine.new").kind,
            SymbolKind::Method
        );
        assert_eq!(
            find(&parsed, "src/engine.rs::Runner.run").kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn rust_fingerprint_survives_renames_but_not_structure() {
        let a = "pub fn f(x: u32) -> u32 { let y = x + 1; y * 2 }\n";
        let renamed = "pub fn f(count: u32) -> u32 { let doubled = count + 1; doubled * 2 }\n";
        let restructured = "pub fn f(x: u32) -> u32 { if x > 0 { x } else { 0 } }\n";
        let fp = |s: &str| {
            parse_source("m.rs", s).unwrap().symbols[0]
                .fingerprint
                .hash
                .clone()
        };
        assert_eq!(
            fp(a),
            fp(renamed),
            "identifier renames must not change the fingerprint"
        );
        assert_ne!(
            fp(a),
            fp(restructured),
            "structural change must change the fingerprint"
        );
    }

    #[test]
    fn rust_generic_impl_methods_share_the_base_type_identity() {
        let src = "struct Wrap<T> { v: T }\nimpl<T> Wrap<T> { fn get(&self) -> &T { &self.v } }\n";
        let parsed = parse_source("w.rs", src).unwrap();
        let qpaths = paths(&parsed);
        assert!(qpaths.contains(&"w.rs::Wrap.get"), "have {qpaths:?}");
    }

    #[test]
    fn typescript_extracts_functions_arrows_classes_and_methods() {
        let src = r#"
export function add(a: number, b: number): number {
    return a + b;
}

const mul = (a: number, b: number): number => a * b;

export class Counter {
    private n = 0;
    increment(): void {
        this.n += 1;
    }
}
"#;
        let parsed = parse_source("src/math.ts", src).unwrap();
        assert_eq!(parsed.lang.as_ref().unwrap().as_str(), "typescript");
        let qpaths = paths(&parsed);
        assert!(qpaths.contains(&"src/math.ts::add"), "have {qpaths:?}");
        assert!(qpaths.contains(&"src/math.ts::mul"), "have {qpaths:?}");
        assert!(qpaths.contains(&"src/math.ts::Counter"), "have {qpaths:?}");
        assert!(
            qpaths.contains(&"src/math.ts::Counter.increment"),
            "have {qpaths:?}"
        );
        assert_eq!(find(&parsed, "src/math.ts::mul").kind, SymbolKind::Function);
        assert_eq!(
            find(&parsed, "src/math.ts::Counter.increment").kind,
            SymbolKind::Method
        );
    }

    #[test]
    fn fingerprint_is_stable_under_variable_rename() {
        // Same structure, different local-variable and parameter names + literal value.
        let v1 = parse_source("m.py", "def f(x):\n    y = x + 1\n    return y\n").unwrap();
        let v2 = parse_source(
            "m.py",
            "def f(value):\n    result = value + 2\n    return result\n",
        )
        .unwrap();
        let f1 = &find(&v1, "m.py::f").fingerprint;
        let f2 = &find(&v2, "m.py::f").fingerprint;
        assert_eq!(
            f1.hash, f2.hash,
            "renaming locals/parameters and changing a literal must not change the structural hash"
        );
    }

    #[test]
    fn fingerprint_changes_on_structural_change() {
        let v1 = parse_source("m.py", "def f(x):\n    return x\n").unwrap();
        let v2 = parse_source(
            "m.py",
            "def f(x):\n    if x:\n        return x\n    return 0\n",
        )
        .unwrap();
        let f1 = &find(&v1, "m.py::f").fingerprint;
        let f2 = &find(&v2, "m.py::f").fingerprint;
        assert_ne!(
            f1.hash, f2.hash,
            "adding control flow must change the structural hash"
        );
    }

    #[test]
    fn fingerprint_survives_rename_of_symbol_itself() {
        // The function's own name changes; body structure identical → fingerprint equal.
        // This is what lets the identity layer treat a rename as the same node.
        let v1 = parse_source("m.py", "def old_name(x):\n    return x + 1\n").unwrap();
        let v2 = parse_source("m.py", "def new_name(x):\n    return x + 1\n").unwrap();
        let f1 = &find(&v1, "m.py::old_name").fingerprint;
        let f2 = &find(&v2, "m.py::new_name").fingerprint;
        assert_eq!(f1.hash, f2.hash);
    }

    #[test]
    fn file_fallback_for_unknown_extension() {
        let parsed = parse_source("README.md", "# Title\nsome text\n").unwrap();
        assert!(parsed.is_file_fallback());
        assert!(parsed.lang.is_none());
        assert!(parsed.symbols.is_empty());
        assert_eq!(parsed.file_fingerprint.hash.len(), 32);
    }

    #[test]
    fn parsing_is_deterministic() {
        let a = parse_source("m.py", "def f():\n    return 1\n").unwrap();
        let b = parse_source("m.py", "def f():\n    return 1\n").unwrap();
        assert_eq!(a, b);
    }
}
