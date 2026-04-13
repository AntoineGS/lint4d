// Skeleton helper; not yet wired into the renderer pipeline.
// Phase F follow-up: integrate with renderer or delete.
#![allow(dead_code)]

use crate::config::IndentStyle;

#[derive(Debug, Clone)]
pub struct IndentContext {
    level: usize,
    size: usize,
    style: IndentStyle,
}

impl IndentContext {
    pub fn new(size: usize, style: IndentStyle) -> Self {
        IndentContext {
            level: 0,
            size,
            style,
        }
    }

    pub fn indent(&mut self) {
        self.level += 1;
    }

    pub fn dedent(&mut self) {
        self.level = self.level.saturating_sub(1);
    }

    pub fn current(&self) -> String {
        match self.style {
            IndentStyle::Space => " ".repeat(self.level * self.size),
            IndentStyle::Tab => "\t".repeat(self.level),
        }
    }

    pub fn continuation(&self) -> String {
        match self.style {
            IndentStyle::Space => " ".repeat((self.level + 1) * self.size),
            IndentStyle::Tab => "\t".repeat(self.level + 1),
        }
    }

    pub fn level(&self) -> usize {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_and_dedent() {
        let mut ctx = IndentContext::new(2, IndentStyle::Space);
        assert_eq!(ctx.current(), "");
        ctx.indent();
        assert_eq!(ctx.current(), "  ");
        ctx.indent();
        assert_eq!(ctx.current(), "    ");
        ctx.dedent();
        assert_eq!(ctx.current(), "  ");
    }

    #[test]
    fn indent_with_tabs() {
        let mut ctx = IndentContext::new(1, IndentStyle::Tab);
        ctx.indent();
        assert_eq!(ctx.current(), "\t");
        ctx.indent();
        assert_eq!(ctx.current(), "\t\t");
    }

    #[test]
    fn continuation_indent() {
        let mut ctx = IndentContext::new(2, IndentStyle::Space);
        ctx.indent();
        assert_eq!(ctx.continuation(), "    "); // level 1 (2) + 1 extra (2) = 4 spaces
    }

    #[test]
    fn dedent_at_zero() {
        let mut ctx = IndentContext::new(2, IndentStyle::Space);
        ctx.dedent();
        assert_eq!(ctx.level(), 0);
    }
}
