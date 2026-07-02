//! Structural AST fingerprinting.
//!
//! A fingerprint captures the *shape* of a piece of code while ignoring the text of
//! identifiers and literals. This is what makes symbol identity survive variable renames
//! and lets the identity layer detect a renamed/moved symbol as the *same* node
//!.
//!
//! Two products are computed:
//! * [`Fingerprint::hash`] — an exact structural hash (sensitive to the full tree shape),
//!   used for the fast exact-match path and stored on each version.
//! * [`Fingerprint::shingles`] — a set of k-gram hashes over the node-kind stream, used by
//!   the identity layer for fuzzy (Jaccard) similarity when a symbol is renamed/moved or
//!   lightly edited.

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

/// k-gram size for shingling the node-kind stream.
const SHINGLE_K: usize = 3;

/// The structural signature of a symbol or file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Exact structural hash (32 lowercase hex chars).
    pub hash: String,
    /// Sorted, deduplicated k-gram hashes for fuzzy similarity.
    pub shingles: Vec<u64>,
}

impl Fingerprint {
    /// Compute the structural fingerprint of an AST subtree rooted at `node`.
    ///
    /// Only *named* nodes contribute (punctuation/keywords-as-anonymous-tokens are
    /// ignored), and only node *kinds* are used — never the source text — so renaming an
    /// identifier or changing a literal leaves the fingerprint unchanged.
    #[must_use]
    pub fn of_node(node: Node) -> Self {
        let mut tokens: Vec<u64> = Vec::new();
        let mut hash_buf: Vec<u8> = Vec::new();
        dfs_named(node, 0, &mut tokens, &mut hash_buf);
        Self {
            hash: hex16(blake3::hash(&hash_buf).as_bytes()),
            shingles: shingles(&tokens, SHINGLE_K),
        }
    }

    /// Compute a content-based fingerprint for a whole file when no grammar is available.
    ///
    /// The exact hash is over the raw bytes; the shingles are per-line hashes, so two
    /// copies of a moved file with mostly-identical lines score as highly similar.
    #[must_use]
    pub fn of_text(source: &str) -> Self {
        let mut line_hashes: Vec<u64> = source
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                u64_prefix(blake3::hash(trimmed.as_bytes()).as_bytes())
            })
            .collect();
        line_hashes.sort_unstable();
        line_hashes.dedup();
        Self {
            hash: hex16(blake3::hash(source.as_bytes()).as_bytes()),
            shingles: line_hashes,
        }
    }
}

/// Pre-order DFS over named nodes, accumulating both the kind-token stream (for shingles)
/// and a depth-annotated byte buffer (for the exact hash).
fn dfs_named(node: Node, depth: u8, tokens: &mut Vec<u64>, hash_buf: &mut Vec<u8>) {
    let child_depth = if node.is_named() {
        let token = kind_token(node.kind());
        tokens.push(token);
        hash_buf.push(depth);
        hash_buf.extend_from_slice(&token.to_be_bytes());
        depth.saturating_add(1)
    } else {
        depth
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        dfs_named(child, child_depth, tokens, hash_buf);
    }
}

/// Stable 64-bit token for a grammar node-kind *name* (not its numeric id, which can vary
/// across grammar versions). Two machines on the same grammar always agree.
fn kind_token(kind: &str) -> u64 {
    u64_prefix(blake3::hash(kind.as_bytes()).as_bytes())
}

/// k-gram shingles over the token stream. For streams shorter than `k`, the individual
/// tokens are used so that even tiny symbols still get a non-empty signature.
fn shingles(tokens: &[u64], k: usize) -> Vec<u64> {
    let mut set: Vec<u64> = if tokens.len() < k {
        tokens.to_vec()
    } else {
        tokens
            .windows(k)
            .map(|window| {
                let mut hasher = blake3::Hasher::new();
                for token in window {
                    hasher.update(&token.to_be_bytes());
                }
                u64_prefix(hasher.finalize().as_bytes())
            })
            .collect()
    };
    set.sort_unstable();
    set.dedup();
    set
}

fn u64_prefix(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(buf)
}

fn hex16(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in &bytes[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_text_is_deterministic() {
        let a = Fingerprint::of_text("line1\nline2\n");
        let b = Fingerprint::of_text("line1\nline2\n");
        assert_eq!(a, b);
        assert_eq!(a.hash.len(), 32);
    }

    #[test]
    fn of_text_similar_for_shared_lines() {
        let a = Fingerprint::of_text("alpha\nbeta\ngamma\n");
        let b = Fingerprint::of_text("alpha\nbeta\ngamma\ndelta\n");
        // They share three line hashes.
        let shared = a.shingles.iter().filter(|s| b.shingles.contains(s)).count();
        assert!(shared >= 3);
    }
}
