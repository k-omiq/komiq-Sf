//! Numbered SQL parameters (`$1, $2, …`), valid for both PostgreSQL and SQLite.

/// Render `count` parameters starting at the one-based argument index `start`.
/// SQL values always travel through binds; this helper emits only parameter names.
pub(crate) fn placeholders(start: usize, count: usize) -> String {
    assert!(start > 0, "SQL argument indices are one-based");
    (start..start + count)
        .map(|index| format!("${index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_lists_cover_empty_offset_and_large_cohorts() {
        assert_eq!(placeholders(1, 0), "");
        assert_eq!(placeholders(3, 3), "$3, $4, $5");
        let list = placeholders(2, 2000);
        assert_eq!(list.split(", ").count(), 2000);
        assert!(list.ends_with("$2001"));
    }
}
