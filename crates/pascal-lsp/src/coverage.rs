//! Coverage of read-only answers.
//!
//! Edits must prove every occurrence before they are offered. Read-only
//! answers (references, workspace symbols, workspace diagnostics) instead
//! return every fact they did prove and record here what they could not
//! check. Every item in such an answer is proven even when the coverage is
//! incomplete; the gaps only describe what may be missing.

use lsp_types::Url;

/// Gaps beyond this many are counted but not retained, so a workspace where
/// every unit is unprovable cannot grow the result without bound.
const MAX_RETAINED_GAPS: usize = 32;
/// Gaps named in the one-line client log.
const MAX_LOGGED_GAPS: usize = 8;

/// One reason a read-only answer may be missing items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoverageGap {
    /// The unit whose facts were withheld, when the gap is local to one.
    pub(crate) uri: Option<Url>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub(crate) gaps: Vec<CoverageGap>,
    /// Distinct gaps past `MAX_RETAINED_GAPS`.
    pub(crate) omitted: usize,
}

impl Coverage {
    pub(crate) fn is_complete(&self) -> bool {
        self.gaps.is_empty() && self.omitted == 0
    }

    pub(crate) fn note(&mut self, uri: Option<&Url>, reason: impl Into<String>) {
        let gap = CoverageGap {
            uri: uri.cloned(),
            reason: reason.into(),
        };
        if self.gaps.contains(&gap) {
            return;
        }
        if self.gaps.len() < MAX_RETAINED_GAPS {
            self.gaps.push(gap);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    /// One log line naming the request and the first gaps.
    pub(crate) fn summary(&self, method: &str) -> String {
        let mut message = format!(
            "pascal-lsp: {method} returned an incomplete result; items that could not be proven were left out:"
        );
        for gap in self.gaps.iter().take(MAX_LOGGED_GAPS) {
            message.push_str("\n  - ");
            if let Some(uri) = &gap.uri {
                message.push_str(uri.as_str());
                message.push_str(": ");
            }
            message.push_str(&gap.reason);
        }
        let unlisted = self
            .gaps
            .len()
            .saturating_sub(MAX_LOGGED_GAPS)
            .saturating_add(self.omitted);
        if unlisted > 0 {
            message.push_str(&format!("\n  - and {unlisted} more"));
        }
        message
    }
}

/// A read-only answer and what it could not prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Partial<T> {
    pub(crate) value: T,
    pub(crate) coverage: Coverage,
}

impl<T> Partial<T> {
    pub(crate) fn complete(value: T) -> Self {
        Self {
            value,
            coverage: Coverage::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str) -> Url {
        Url::parse(&format!("file:///workspace/{name}.pas")).unwrap()
    }

    #[test]
    fn coverage_is_complete_until_a_gap_is_noted() {
        let mut coverage = Coverage::default();
        assert!(coverage.is_complete());
        coverage.note(Some(&unit("Legacy")), "unsupported directive");
        coverage.note(Some(&unit("Legacy")), "unsupported directive");
        assert!(!coverage.is_complete());
        assert_eq!(coverage.gaps.len(), 1);
    }

    #[test]
    fn coverage_counts_gaps_past_the_retention_limit() {
        let mut coverage = Coverage::default();
        for index in 0..MAX_RETAINED_GAPS + 5 {
            coverage.note(Some(&unit(&format!("Unit{index}"))), "parser recovery");
        }
        assert_eq!(coverage.gaps.len(), MAX_RETAINED_GAPS);
        assert_eq!(coverage.omitted, 5);
        let summary = coverage.summary("textDocument/references");
        assert!(summary.contains("textDocument/references"), "{summary}");
        assert!(summary.contains("Unit0.pas"), "{summary}");
        assert!(
            summary.contains(&format!(
                "and {} more",
                MAX_RETAINED_GAPS + 5 - MAX_LOGGED_GAPS
            )),
            "{summary}"
        );
    }
}
