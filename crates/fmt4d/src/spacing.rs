/// Returns true if a space should appear BEFORE a token of this kind.
pub fn space_before(kind: &str) -> bool {
    matches!(
        kind,
        "kAssign"
            | "kAssignAdd"
            | "kAssignSub"
            | "kAssignMul"
            | "kAssignDiv"
            | "kAdd"
            | "kSub"
            | "kMul"
            | "kDiv"
            | "kMod"
            | "kAnd"
            | "kOr"
            | "kXor"
            | "kShl"
            | "kShr"
            | "="
            | "<>"
            | "<"
            | ">"
            | "<="
            | ">="
            | "kIn"
            | "kIs"
            | "kAs"
            | "kThen"
            | "kDo"
            | "kOf"
    )
}

/// Returns true if a space should appear AFTER a token of this kind.
pub fn space_after(kind: &str) -> bool {
    matches!(
        kind,
        "kAssign"
            | "kAssignAdd"
            | "kAssignSub"
            | "kAssignMul"
            | "kAssignDiv"
            | "kAdd"
            | "kSub"
            | "kMul"
            | "kDiv"
            | "kMod"
            | "kAnd"
            | "kOr"
            | "kXor"
            | "kShl"
            | "kShr"
            | "="
            | "<>"
            | "<"
            | ">"
            | "<="
            | ">="
            | "kIn"
            | "kIs"
            | "kAs"
            | "kIf"
            | "kWhile"
            | "kFor"
            | "kCase"
            | "kUntil"
            | "kExcept"
            | "kOn"
            | "kNot"
            | ","
            | ":"
    )
}

/// Context-aware space-before check.
pub fn space_before_in_context(kind: &str, parent_kind: &str) -> bool {
    if kind == "(" && (parent_kind == "exprCall" || parent_kind == "declArgs") {
        return false;
    }
    space_before(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_around_assign() {
        assert!(space_before("kAssign"));
        assert!(space_after("kAssign"));
    }

    #[test]
    fn space_after_comma() {
        assert!(!space_before(","));
        assert!(space_after(","));
    }

    #[test]
    fn no_space_before_semicolon() {
        assert!(!space_before(";"));
    }

    #[test]
    fn space_after_keywords() {
        assert!(space_after("kIf"));
        assert!(space_after("kWhile"));
        assert!(space_after("kFor"));
    }

    #[test]
    fn no_space_before_paren_in_call() {
        assert!(!space_before_in_context("(", "exprCall"));
        assert!(!space_before_in_context("(", "declArgs"));
    }

    #[test]
    fn no_space_inside_parens() {
        assert!(!space_after("("));
        assert!(!space_before(")"));
    }
}
