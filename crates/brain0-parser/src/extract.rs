//! Symbol extraction rules per language family.
//!
//! The walk descends the AST maintaining a scope stack (enclosing class/function names)
//! so each symbol gets a deterministic, hierarchical `qualified_path` of the form
//! `"<rel_path>::<Outer>.<inner>"`. Where the grammar exposes functions/classes/methods we
//! emit symbol-level units; the file-level fallback is handled by the caller in `lib.rs`.

use tree_sitter::{Language, Node};

use crate::fingerprint::Fingerprint;
use crate::{ExtractedSymbol, SymbolKind};

/// Language family, which determines extraction rules. TypeScript and JavaScript share
/// rules (the relevant node kinds coincide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Python,
    Ecmascript,
    Rust,
}

/// A selected grammar plus its model-facing language name.
pub(crate) struct LangSel {
    pub family: Family,
    pub lang_name: &'static str,
    pub language: Language,
}

/// Map a repo-relative path to a grammar, or `None` to trigger the file-level fallback.
pub(crate) fn detect_language(rel_path: &str) -> Option<LangSel> {
    let ext = rel_path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "py" | "pyi" => Some(LangSel {
            family: Family::Python,
            lang_name: "python",
            language: tree_sitter_python::LANGUAGE.into(),
        }),
        "ts" | "mts" | "cts" => Some(LangSel {
            family: Family::Ecmascript,
            lang_name: "typescript",
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }),
        "tsx" => Some(LangSel {
            family: Family::Ecmascript,
            lang_name: "typescript",
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
        }),
        "js" | "mjs" | "cjs" | "jsx" => Some(LangSel {
            family: Family::Ecmascript,
            lang_name: "javascript",
            language: tree_sitter_javascript::LANGUAGE.into(),
        }),
        "rs" => Some(LangSel {
            family: Family::Rust,
            lang_name: "rust",
            language: tree_sitter_rust::LANGUAGE.into(),
        }),
        _ => None,
    }
}

struct Frame {
    name: String,
    is_class: bool,
}

struct Def<'tree> {
    name: String,
    kind: SymbolKind,
    /// Subtree used for the structural fingerprint (the function/class body).
    fingerprint_node: Node<'tree>,
    /// Subtree used for the reported source span.
    span_node: Node<'tree>,
    /// When false the definition only contributes scope (qualified-path prefix) without
    /// emitting a symbol — e.g. a Rust `impl` block: its methods are `Type.method`, but the
    /// type identity belongs to the `struct`/`enum` item, and several impl blocks for one
    /// type must not collide on the same qualified path.
    emit: bool,
}

pub(crate) struct Extractor<'a> {
    rel_path: &'a str,
    source: &'a [u8],
    family: Family,
    symbols: Vec<ExtractedSymbol>,
}

impl<'a> Extractor<'a> {
    pub(crate) fn new(rel_path: &'a str, source: &'a [u8], family: Family) -> Self {
        Self {
            rel_path,
            source,
            family,
            symbols: Vec::new(),
        }
    }

    pub(crate) fn run(mut self, root: Node) -> Vec<ExtractedSymbol> {
        let mut scope: Vec<Frame> = Vec::new();
        self.walk(root, &mut scope);
        self.symbols
    }

    fn walk(&mut self, node: Node, scope: &mut Vec<Frame>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(def) = self.classify(child, scope) {
                let is_class = matches!(def.kind, SymbolKind::Class);
                scope.push(Frame {
                    name: def.name.clone(),
                    is_class,
                });
                if def.emit {
                    let dotted = scope
                        .iter()
                        .map(|frame| frame.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    let span = def.span_node;
                    self.symbols.push(ExtractedSymbol {
                        kind: def.kind,
                        name: def.name,
                        qualified_path: format!("{}::{}", self.rel_path, dotted),
                        start_byte: span.start_byte(),
                        end_byte: span.end_byte(),
                        start_line: span.start_position().row + 1,
                        end_line: span.end_position().row + 1,
                        fingerprint: Fingerprint::of_node(def.fingerprint_node),
                    });
                }
                // Recurse into the definition to capture nested symbols (methods, closures).
                self.walk(child, scope);
                scope.pop();
            } else {
                self.walk(child, scope);
            }
        }
    }

    fn classify<'t>(&self, node: Node<'t>, scope: &[Frame]) -> Option<Def<'t>> {
        let enclosing_class = scope.last().is_some_and(|frame| frame.is_class);
        match self.family {
            Family::Python => self.classify_python(node, enclosing_class),
            Family::Ecmascript => self.classify_ecmascript(node, enclosing_class),
            Family::Rust => self.classify_rust(node, enclosing_class),
        }
    }

    fn classify_rust<'t>(&self, node: Node<'t>, enclosing_class: bool) -> Option<Def<'t>> {
        match node.kind() {
            "function_item" => Some(Def {
                name: self.name_of(node)?,
                kind: if enclosing_class {
                    SymbolKind::Method // associated fn / method inside impl or trait
                } else {
                    SymbolKind::Function
                },
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            // Type definitions own the type's identity (class-like for scoping).
            "struct_item" | "enum_item" | "trait_item" | "union_item" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Class,
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            // An impl block is SCOPE ONLY: its methods become `Type.method`, but the type
            // symbol belongs to the struct/enum item, and a type commonly has several impl
            // blocks — emitting each would collide on one qualified path.
            "impl_item" => {
                let type_node = node.child_by_field_name("type")?;
                let name = type_node
                    .utf8_text(self.source)
                    .ok()?
                    .split('<') // drop generics: `Foo<T>` and `Foo<U>` are the same identity
                    .next()?
                    .trim()
                    .to_owned();
                Some(Def {
                    name,
                    kind: SymbolKind::Class,
                    fingerprint_node: node,
                    span_node: node,
                    emit: false,
                })
            }
            // Modules scope their contents (e.g. `mod tests`) without emitting a symbol —
            // `file.rs::tests.helper` stays distinct from a top-level `file.rs::helper`.
            "mod_item" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Function, // not class-like: a mod's fns are not methods
                fingerprint_node: node,
                span_node: node,
                emit: false,
            }),
            _ => None,
        }
    }

    fn classify_python<'t>(&self, node: Node<'t>, enclosing_class: bool) -> Option<Def<'t>> {
        match node.kind() {
            "function_definition" => Some(Def {
                name: self.name_of(node)?,
                kind: if enclosing_class {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            "class_definition" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Class,
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            _ => None,
        }
    }

    fn classify_ecmascript<'t>(&self, node: Node<'t>, enclosing_class: bool) -> Option<Def<'t>> {
        match node.kind() {
            "function_declaration" | "generator_function_declaration" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Function,
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            "class_declaration" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Class,
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            "method_definition" => Some(Def {
                name: self.name_of(node)?,
                kind: SymbolKind::Method,
                fingerprint_node: node,
                span_node: node,
                emit: true,
            }),
            // `const f = () => {}` / `let g = function () {}`
            "variable_declarator" => {
                let value = node.child_by_field_name("value")?;
                if is_function_value(value.kind()) {
                    Some(Def {
                        name: self.name_of(node)?,
                        kind: if enclosing_class {
                            SymbolKind::Method
                        } else {
                            SymbolKind::Function
                        },
                        fingerprint_node: value,
                        span_node: node,
                        emit: true,
                    })
                } else {
                    None
                }
            }
            // class property assigned an arrow function → method
            "public_field_definition" | "field_definition" => {
                let value = node.child_by_field_name("value")?;
                if is_function_value(value.kind()) {
                    Some(Def {
                        name: self.name_of(node)?,
                        kind: SymbolKind::Method,
                        fingerprint_node: value,
                        span_node: node,
                        emit: true,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn name_of(&self, node: Node) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        name_node.utf8_text(self.source).ok().map(ToOwned::to_owned)
    }
}

fn is_function_value(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function" | "function_expression" | "generator_function"
    )
}
