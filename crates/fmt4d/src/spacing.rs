use pascal_core::node_kind as K;

/// Returns true if a space should appear BEFORE a token of this kind.
pub fn space_before(kind: &str) -> bool {
    matches!(
        kind,
        K::K_ASSIGN
            | K::K_ASSIGN_ADD
            | K::K_ASSIGN_SUB
            | K::K_ASSIGN_MUL
            | K::K_ASSIGN_DIV
            | K::K_ADD
            | K::K_SUB
            | K::K_MUL
            | K::K_DIV
            | K::K_MOD
            | K::K_AND
            | K::K_OR
            | K::K_XOR
            | K::K_SHL
            | K::K_SHR
            | K::EQUALS
            | K::NOT_EQUALS
            | K::LESS_THAN
            | K::GREATER_THAN
            | K::LESS_EQUAL
            | K::GREATER_EQUAL
            | K::K_IN
            | K::K_IS
            | K::K_AS
            | K::K_THEN
            | K::K_DO
            | K::K_OF
    )
}

/// Returns true if a space should appear AFTER a token of this kind.
pub fn space_after(kind: &str) -> bool {
    matches!(
        kind,
        K::K_ASSIGN
            | K::K_ASSIGN_ADD
            | K::K_ASSIGN_SUB
            | K::K_ASSIGN_MUL
            | K::K_ASSIGN_DIV
            | K::K_ADD
            | K::K_SUB
            | K::K_MUL
            | K::K_DIV
            | K::K_MOD
            | K::K_AND
            | K::K_OR
            | K::K_XOR
            | K::K_SHL
            | K::K_SHR
            | K::EQUALS
            | K::NOT_EQUALS
            | K::LESS_THAN
            | K::GREATER_THAN
            | K::LESS_EQUAL
            | K::GREATER_EQUAL
            | K::K_IN
            | K::K_IS
            | K::K_AS
            | K::K_IF
            | K::K_WHILE
            | K::K_FOR
            | K::K_CASE
            | K::K_UNTIL
            | K::K_EXCEPT
            | K::K_ON
            | K::K_NOT
            | K::COMMA
            | K::COLON
    )
}

/// Context-aware space-before check.
pub fn space_before_in_context(kind: &str, parent_kind: &str) -> bool {
    if kind == K::OPEN_PAREN && (parent_kind == K::EXPR_CALL || parent_kind == K::DECL_ARGS) {
        return false;
    }
    space_before(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_around_assign() {
        assert!(space_before(K::K_ASSIGN));
        assert!(space_after(K::K_ASSIGN));
    }

    #[test]
    fn space_after_comma() {
        assert!(!space_before(K::COMMA));
        assert!(space_after(K::COMMA));
    }

    #[test]
    fn no_space_before_semicolon() {
        assert!(!space_before(K::SEMICOLON));
    }

    #[test]
    fn space_after_keywords() {
        assert!(space_after(K::K_IF));
        assert!(space_after(K::K_WHILE));
        assert!(space_after(K::K_FOR));
    }

    #[test]
    fn no_space_before_paren_in_call() {
        assert!(!space_before_in_context(K::OPEN_PAREN, K::EXPR_CALL));
        assert!(!space_before_in_context(K::OPEN_PAREN, K::DECL_ARGS));
    }

    #[test]
    fn no_space_inside_parens() {
        assert!(!space_after(K::OPEN_PAREN));
        assert!(!space_before(K::CLOSE_PAREN));
    }
}
