//! The delimited-list columns, and which delimiter each one uses.
//!
//! Several columns are joined strings rather than richer types, and they do
//! not agree on the delimiter: the package columns use `;`, the worker columns
//! `,`. Every split and join goes through [`ListColumn`], so a value can only
//! be read the way it was written -- a dump once split the worker columns on
//! `;`, and restored a hand-edited worker as one unknown architecture.

/// A kind of delimited-list column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListColumn {
    /// `packages.platforms` and `packages.build_flags`, `;`-joined.
    Package,
    /// `workers.native_arches`, `emulated_arches` and `package_affinity`,
    /// `,`-joined.
    Worker,
}

impl ListColumn {
    const fn delimiter(self) -> char {
        match self {
            Self::Package => ';',
            Self::Worker => ',',
        }
    }

    /// The entries of a stored value, trimmed, with empties dropped: an empty
    /// column is no entries, not one empty entry, and a stray space is not
    /// part of a name.
    #[must_use]
    pub fn split(self, value: &str) -> Vec<String> {
        value
            .split(self.delimiter())
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    /// The stored form of `values`.
    #[must_use]
    pub fn join<S: AsRef<str>>(self, values: &[S]) -> String {
        let mut joined = String::new();
        for value in values {
            if !joined.is_empty() {
                joined.push(self.delimiter());
            }
            joined.push_str(value.as_ref());
        }
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::ListColumn;

    #[test]
    fn an_empty_column_means_no_entries() {
        assert!(ListColumn::Package.split("").is_empty());
        assert_eq!(ListColumn::Package.split("x86_64"), vec!["x86_64"]);
        assert_eq!(
            ListColumn::Package.split("x86_64;;aarch64"),
            vec!["x86_64", "aarch64"]
        );
    }

    #[test]
    fn entries_are_trimmed() {
        assert_eq!(
            ListColumn::Package.split("x86_64; aarch64 "),
            vec!["x86_64", "aarch64"]
        );
        assert_eq!(ListColumn::Worker.split("a, b ,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_list_reads_back_as_it_was_written() {
        for column in [ListColumn::Package, ListColumn::Worker] {
            let values = ["x86_64", "aarch64"];
            assert_eq!(column.split(&column.join(&values)), values);
        }
        assert_eq!(
            ListColumn::Worker.join(&["x86_64", "aarch64"]),
            "x86_64,aarch64"
        );
        assert_eq!(ListColumn::Package.join(&["a", "b"]), "a;b");
    }
}
