//! The identifier a script uses for a tool (upstream `identifier.ts`).

/// Characters that are not valid in a JavaScript identifier become `_`:
/// `mcp__docs__search` stays as is, `my-tool` becomes `my_tool`, and an
/// empty name is `_`. Only ASCII letters, digits, `_` and `$` are kept, as
/// upstream's regular expressions keep them.
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for c in name.chars() {
        let valid = if identifier.is_empty() {
            c.is_ascii_alphabetic() || c == '_' || c == '$'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        };
        identifier.push(if valid { c } else { '_' });
    }
    if identifier.is_empty() {
        identifier.push('_');
    }
    identifier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_characters_become_underscores() {
        assert_eq!(
            to_codemode_identifier("mcp__docs__search"),
            "mcp__docs__search"
        );
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("9lives"), "_lives");
        assert_eq!(to_codemode_identifier("$ok1"), "$ok1");
        assert_eq!(to_codemode_identifier("a.b c"), "a_b_c");
        // One `_` per character, as upstream's `for...of` walks code points.
        assert_eq!(to_codemode_identifier("é1"), "_1");
        assert_eq!(to_codemode_identifier(""), "_");
    }
}
