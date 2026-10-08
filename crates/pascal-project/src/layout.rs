//! Immutable Delphi build-target facts and source-entry layout settings.
//!
//! These are data, not a storage-layout solver. Missing values are unproven;
//! neither mutable Pascal defines nor the host ABI supply target facts.

use crate::{CompilerVersion, ConditionalFact, TargetPlatform};

/// Selected Delphi target, including targets with no verified layout profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LayoutPlatform {
    Win32,
    Win64,
    Other(String),
}

/// Source-entry settings; explicit settings do not establish ABI support.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayoutSettings {
    pub record_alignment: Option<u8>,
    pub minimum_enum_size: Option<u8>,
    pub long_strings: ConditionalFact,
    pub old_type_layout: ConditionalFact,
    pub real_compatibility: ConditionalFact,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            record_alignment: None,
            minimum_enum_size: None,
            long_strings: ConditionalFact::Unknown,
            old_type_layout: ConditionalFact::Unknown,
            real_compatibility: ConditionalFact::Unknown,
        }
    }
}

/// Target and defaults for one source origin, separate from conditional defines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LayoutContext {
    pub platform: Option<LayoutPlatform>,
    pub defaults: LayoutSettings,
}

impl LayoutContext {
    /// Retain the selected target and admit only independently referenced defaults.
    /// The caller must first validate its build selection.
    ///
    /// The sole admitted default profile is Delphi 12 Athens (CompilerVersion
    /// 36.0), Win32/Win64: A8 and H+. Official indexed default statements:
    /// <https://docwiki.embarcadero.com/RADStudio/Athens/en/Align_fields_(Delphi)>
    /// and <https://docwiki.embarcadero.com/RADStudio/Athens/en/Long_strings_(Delphi)>.
    /// No enum/legacy defaults, other versions, or ALIGN 16 applicability are
    /// inferred from those statements. In particular, Florence/37 stays unknown.
    pub fn for_target(version: Option<CompilerVersion>, platform: Option<&TargetPlatform>) -> Self {
        let platform = platform.map(|target| match target {
            TargetPlatform::Win32 => LayoutPlatform::Win32,
            TargetPlatform::Win64 => LayoutPlatform::Win64,
            TargetPlatform::Other(name) => LayoutPlatform::Other(name.clone()),
        });
        let mut defaults = LayoutSettings::default();
        if version == Some(CompilerVersion::new(36, 0))
            && matches!(
                platform,
                Some(LayoutPlatform::Win32 | LayoutPlatform::Win64)
            )
        {
            defaults.record_alignment = Some(8);
            defaults.long_strings = ConditionalFact::True;
        }
        Self { platform, defaults }
    }
}
