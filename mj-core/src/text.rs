//! Small helpers for the words Mjolnir shows people.

/// `count` and the noun that agrees with it: `1 command`, `2 commands`,
/// `0 entries`. Counts shown to a person go through this, so none reads
/// "1 commands" or "2 session(s)" (launch findings C-12 and R5-10).
pub fn counted<T>(count: T, singular: &str, plural: &str) -> String
where
    T: std::fmt::Display + PartialEq + From<u8>,
{
    let noun = if count == T::from(1) {
        singular
    } else {
        plural
    };
    format!("{count} {noun}")
}

#[cfg(test)]
mod tests {
    use super::counted;

    #[test]
    fn counted_agrees_the_noun_with_the_count() {
        assert_eq!(counted(0_usize, "entry", "entries"), "0 entries");
        assert_eq!(counted(1_usize, "entry", "entries"), "1 entry");
        assert_eq!(counted(2_i64, "command", "commands"), "2 commands");
    }
}
