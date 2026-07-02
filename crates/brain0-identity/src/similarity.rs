//! Set similarity over fingerprint shingles.

/// Jaccard similarity of two **sorted, deduplicated** shingle sets (as produced by
/// [`brain0_parser::Fingerprint`]). Returns a value in `0.0..=1.0`.
///
/// Two empty sets are considered identical (`1.0`); one empty and one non-empty are
/// maximally dissimilar (`0.0`).
#[must_use]
pub fn jaccard(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut i = 0;
    let mut j = 0;
    let mut intersection = 0usize;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sets() {
        assert_eq!(jaccard(&[1, 2, 3], &[1, 2, 3]), 1.0);
        assert_eq!(jaccard(&[], &[]), 1.0);
    }

    #[test]
    fn disjoint_sets() {
        assert_eq!(jaccard(&[1, 2], &[3, 4]), 0.0);
        assert_eq!(jaccard(&[], &[1]), 0.0);
    }

    #[test]
    fn partial_overlap() {
        // intersection {2,3} = 2, union {1,2,3,4} = 4 → 0.5
        assert_eq!(jaccard(&[1, 2, 3], &[2, 3, 4]), 0.5);
    }
}
