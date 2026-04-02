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

/// Parent kinds that indicate a generic type context (not comparison).
pub fn is_generic_parent(parent_kind: &str) -> bool {
    matches!(
        parent_kind,
        K::TYPEREF_TPL | K::GENERIC_TPL | K::GENERIC_ARGS | K::TYPEREF_ARGS | K::EXPR_TPL
    )
}

/// Keywords after which a space is always needed.
pub fn is_keyword_needing_space_after(kind: &str) -> bool {
    matches!(
        kind,
        K::K_PROCEDURE
            | K::K_FUNCTION
            | K::K_CONSTRUCTOR
            | K::K_DESTRUCTOR
            | K::K_CLASS
            | K::K_RECORD
            | K::K_PROPERTY
            | K::K_RAISE
            | K::K_INHERITED
            | K::K_WITH
            | K::K_ARRAY
            | K::K_SET
            | K::K_FILE
            | K::K_STRING
            | K::K_PROGRAM
            | K::K_LIBRARY
            | K::K_UNIT
            | K::K_USES
            | K::K_OF
            | K::K_THEN
            | K::K_DO
            | K::K_TO
            | K::K_DOWNTO
            | K::K_ELSE
    )
}

/// Pure spacing check: should a space appear between two adjacent tokens?
///
/// Takes the kind and parent-kind of both the previous and current token.
/// This is the single source of truth for inter-token spacing decisions.
pub fn would_need_space(
    prev_kind: &str,
    prev_parent_kind: &str,
    kind: &str,
    parent_kind: &str,
) -> bool {
    if prev_kind.is_empty() {
        return false;
    }
    if kind == K::CLOSE_PAREN || kind == K::CLOSE_BRACKET || kind == K::DOT {
        return false;
    }
    if prev_kind == K::OPEN_PAREN || prev_kind == K::OPEN_BRACKET || prev_kind == K::DOT {
        return false;
    }
    if kind == K::SEMICOLON {
        return false;
    }
    if kind == K::COMMA {
        return false;
    }
    if kind == K::OPEN_BRACKET && parent_kind == K::EXPR_SUBSCRIPT {
        return false;
    }
    if kind == K::OPEN_PAREN && (parent_kind == K::EXPR_CALL || parent_kind == K::DECL_ARGS) {
        return false;
    }
    if (kind == K::K_LT || kind == K::LESS_THAN)
        && (parent_kind == K::TYPEREF_TPL
            || parent_kind == K::GENERIC_TPL
            || parent_kind == K::GENERIC_ARGS
            || parent_kind == K::TYPEREF_ARGS
            || parent_kind == K::EXPR_TPL)
    {
        return false;
    }
    if (kind == K::K_GT || kind == K::GREATER_THAN)
        && (parent_kind == K::TYPEREF_TPL
            || parent_kind == K::GENERIC_TPL
            || parent_kind == K::GENERIC_ARGS
            || parent_kind == K::TYPEREF_ARGS
            || parent_kind == K::EXPR_TPL)
    {
        return false;
    }
    if prev_kind == K::K_LT && is_generic_parent(prev_parent_kind) {
        return false;
    }
    if kind == K::COLON {
        return false;
    }
    if kind == K::DOTDOT || prev_kind == K::DOTDOT {
        return false;
    }
    if kind == K::K_DOT {
        return false;
    }
    if prev_kind == K::K_DOT || prev_kind == K::DOT {
        return false;
    }
    if space_before(kind) {
        return true;
    }
    if space_after(prev_kind) {
        return true;
    }
    if is_keyword_needing_space_after(prev_kind) {
        return true;
    }
    if !prev_kind.is_empty()
        && prev_kind != K::SEMICOLON
        && prev_kind != K::OPEN_PAREN
        && prev_kind != K::OPEN_BRACKET
    {
        return true;
    }
    false
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
