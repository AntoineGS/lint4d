// The catalogue lands before the corresponding semantic checks start emitting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub(crate) enum DccMessage {
    E2004,
    E2037,
    E2065,
    E2137,
    E2170,
    E2211,
    E2252,
    E2254,
    E2291,
    E2301,
    H2219,
    W1009,
    W1010,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DccSeverity {
    Error,
    Warning,
    Hint,
}

#[allow(dead_code)]
impl DccMessage {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::E2004 => "E2004",
            Self::E2037 => "E2037",
            Self::E2065 => "E2065",
            Self::E2137 => "E2137",
            Self::E2170 => "E2170",
            Self::E2211 => "E2211",
            Self::E2252 => "E2252",
            Self::E2254 => "E2254",
            Self::E2291 => "E2291",
            Self::E2301 => "E2301",
            Self::H2219 => "H2219",
            Self::W1009 => "W1009",
            Self::W1010 => "W1010",
        }
    }

    pub(crate) fn severity(self) -> DccSeverity {
        match self {
            Self::E2004
            | Self::E2037
            | Self::E2065
            | Self::E2137
            | Self::E2170
            | Self::E2211
            | Self::E2252
            | Self::E2254
            | Self::E2291
            | Self::E2301 => DccSeverity::Error,
            Self::H2219 => DccSeverity::Hint,
            Self::W1009 | Self::W1010 => DccSeverity::Warning,
        }
    }

    pub(crate) fn template(self) -> &'static str {
        match self {
            Self::E2004 => "Identifier redeclared: '%s'",
            Self::E2037 => "Declaration of '%s' differs from previous declaration",
            Self::E2065 => "Unsatisfied forward or external declaration: '%s'",
            Self::E2137 => "Method '%s' not found in base class",
            Self::E2170 => "Cannot override a non-virtual method",
            Self::E2211 => "Declaration of '%s' differs from declaration in interface '%s'",
            Self::E2252 => "Method '%s' with identical parameters already exists",
            Self::E2254 => "Overloaded procedure '%s' must be marked with the 'overload' directive",
            Self::E2291 => "Missing implementation of interface method %s.%s",
            Self::E2301 => "Method '%s' with identical parameters and result type already exists",
            Self::H2219 => "Private symbol '%s' declared but never used",
            Self::W1009 => "Redeclaration of '%s' hides a member in the base class",
            Self::W1010 => "Method '%s' hides virtual method of base type '%s'",
        }
    }

    pub(crate) fn format(self, args: &[&str]) -> String {
        let mut segments = self.template().split("%s");
        let mut formatted = String::with_capacity(self.template().len());
        formatted.push_str(segments.next().unwrap_or_default());
        for (arg, segment) in args.iter().zip(segments.by_ref()) {
            formatted.push_str(arg);
            formatted.push_str(segment);
        }
        for segment in segments {
            formatted.push_str("%s");
            formatted.push_str(segment);
        }
        formatted
    }
}

#[cfg(test)]
mod tests {
    use super::{DccMessage, DccSeverity};

    #[test]
    fn format_replaces_message_placeholders_in_order() {
        assert_eq!(
            DccMessage::E2291.format(&["IFoo", "Bar"]),
            "Missing implementation of interface method IFoo.Bar"
        );
    }

    #[test]
    fn severity_follows_the_code_prefix() {
        assert_eq!(DccMessage::H2219.severity(), DccSeverity::Hint);
        assert_eq!(DccMessage::W1010.severity(), DccSeverity::Warning);
        assert_eq!(DccMessage::E2037.severity(), DccSeverity::Error);
    }

    #[test]
    fn every_message_has_its_exact_code_and_template() {
        let messages = [
            (DccMessage::E2004, "E2004", "Identifier redeclared: '%s'"),
            (
                DccMessage::E2037,
                "E2037",
                "Declaration of '%s' differs from previous declaration",
            ),
            (
                DccMessage::E2065,
                "E2065",
                "Unsatisfied forward or external declaration: '%s'",
            ),
            (
                DccMessage::E2137,
                "E2137",
                "Method '%s' not found in base class",
            ),
            (
                DccMessage::E2170,
                "E2170",
                "Cannot override a non-virtual method",
            ),
            (
                DccMessage::E2211,
                "E2211",
                "Declaration of '%s' differs from declaration in interface '%s'",
            ),
            (
                DccMessage::E2252,
                "E2252",
                "Method '%s' with identical parameters already exists",
            ),
            (
                DccMessage::E2254,
                "E2254",
                "Overloaded procedure '%s' must be marked with the 'overload' directive",
            ),
            (
                DccMessage::E2291,
                "E2291",
                "Missing implementation of interface method %s.%s",
            ),
            (
                DccMessage::E2301,
                "E2301",
                "Method '%s' with identical parameters and result type already exists",
            ),
            (
                DccMessage::H2219,
                "H2219",
                "Private symbol '%s' declared but never used",
            ),
            (
                DccMessage::W1009,
                "W1009",
                "Redeclaration of '%s' hides a member in the base class",
            ),
            (
                DccMessage::W1010,
                "W1010",
                "Method '%s' hides virtual method of base type '%s'",
            ),
        ];

        for (message, expected_code, expected_template) in messages {
            let code = message.code();
            assert_eq!(code, expected_code);
            assert_eq!(code.len(), 5, "{code}");
            assert!(matches!(code.as_bytes()[0], b'E' | b'W' | b'H'), "{code}");
            assert_eq!(message.template(), expected_template, "{code}");
        }
    }
}
