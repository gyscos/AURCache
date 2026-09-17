//! The delimited lists the database stores, as actual lists.
//!
//! Several columns (`platforms`, `build_flags`, arches, affinity) are `;`-
//! or `,`-joined strings rather than richer DB types. Every split goes
//! through here so the rule — trim, drop empties — is stated once: an empty
//! column splits to `[""]`, which would otherwise render as a blank chip, and
//! a stray space would otherwise become part of a name.

/// Split `value` on `delimiter`, trimming each entry and dropping empties.
///
/// An empty column means "no entries", not one empty entry.
#[must_use]
pub fn split_delimited(value: &str, delimiter: char) -> Vec<String> {
    value
        .split(delimiter)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split_delimited;

    /// An empty column is "no entries", not one empty entry — otherwise an
    /// empty `platforms`/`build_flags` column renders as a blank chip.
    #[test]
    fn an_empty_column_means_no_entries() {
        assert!(split_delimited("", ';').is_empty());
        assert_eq!(split_delimited("x86_64", ';'), vec!["x86_64"]);
        assert_eq!(
            split_delimited("x86_64;;aarch64", ';'),
            vec!["x86_64", "aarch64"]
        );
    }

    /// Whitespace around entries is not part of the name, in either dialect.
    #[test]
    fn entries_are_trimmed() {
        assert_eq!(
            split_delimited("x86_64; aarch64 ", ';'),
            vec!["x86_64", "aarch64"]
        );
        assert_eq!(split_delimited("a, b ,c", ','), vec!["a", "b", "c"]);
    }
}
