#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

#[test]
fn imported_alias_layout_uses_provider_scope_and_context() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$DEFINE PRIVATE} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface {$IFDEF PRIVATE} type Export=Byte; {$ELSE} type Export=Int64; {$ENDIF} implementation type Hidden=Byte; end.", context(LayoutPlatform::Win32).with_absent_define(Truth::False));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

#[test]
fn imported_variable_member_index_and_pointer_use_captured_types() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Base=Byte; {$IF SizeOf(Types.Value.Field[0]^)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types", "unit Types; interface type Base=Int64; P=^Base; R=record Field:array[0..1] of P; end; var Value:R; implementation end.", context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn implementation_private_and_unimported_qualifiers_are_unproven() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Hidden)=1}{$DEFINE PRIVATE_HIT}{$ENDIF} {$IF SizeOf(System.Byte)=1}{$DEFINE SYSTEM_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Byte; implementation type Hidden=Byte; end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE PRIVATE_HIT"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE SYSTEM_HIT"), Truth::Unknown);
}

#[test]
fn local_qualifier_shadow_blocks_imported_access() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Types:Byte; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn unknown_import_activity_never_reads_provider() {
    let mut f = Fixture::new(
        "unit Main; interface {$IFDEF OPEN} uses Types; {$ENDIF} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
    assert!(f.read_names().is_empty());
}

#[test]
fn imported_record_alias_member_preserves_provider_graph() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Alias=Types.R; var V:Alias; {$IF SizeOf(V.Field)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type R=record Field:Int64; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn imported_array_selector_uses_importer_index_type() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Index:Integer; {$IF SizeOf(Types.V[Index])=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type A=array[0..1] of Int64; var V:A; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn provider_interface_does_not_read_implementation_includes() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation {$I forbidden.inc} end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    assert!(!f.read_names().contains(&"forbidden.inc".to_string()));
}

#[test]
fn fresh_provider_revision_changes_exported_layout() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
    f.unit(
        "Types",
        "unit Types; interface type Export=Byte; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::False);
}

#[test]
fn recursive_providers_keep_pointer_storage_distinct_from_by_value() {
    let mut f = Fixture::new(
        "unit Main; interface uses A; {$IF SizeOf(A.P)=4}{$DEFINE POINTER_HIT}{$ENDIF} {$IF SizeOf(A.R)=4}{$DEFINE VALUE_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "A",
        "unit A; interface uses B; type P=^B.R; R=record Field:B.R; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "B",
        "unit B; interface uses A; type R=record Field:A.R; end; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE POINTER_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE VALUE_HIT"), Truth::Unknown);
}

#[test]
fn open_provider_condition_does_not_export_matching_size_identity() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types","unit Types; interface {$IFDEF OPEN} type Export=Int64; {$ELSE} type Export=UInt64; {$ENDIF} implementation end.",context(LayoutPlatform::Win32));
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn shared_provider_query_limit_precedes_provider_reads() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_provider_queries = 0;
    assert!(f.analyze().is_err());
    assert!(f.read_names().is_empty());
}

#[test]
fn authorized_namespace_aliases_expose_types_and_variables() {
    let mut f = Fixture::new(
        "unit Main; interface uses Alias; {$IF SizeOf(Alias.Export)=8}{$DEFINE TYPE_HIT}{$ENDIF} {$IF SizeOf(Vendor.Types.V)=8}{$DEFINE VAR_HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Alias",
        "unit Vendor.Types; interface type Export=Int64; var V:Export; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit_qualifiers("Alias", &["Alias", "Vendor.Types"]);
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE TYPE_HIT"), Truth::True);
    assert_eq!(activity(&a, "DEFINE VAR_HIT"), Truth::True);
}

#[test]
fn unavailable_and_ambiguous_providers_do_not_read_or_prove_layout() {
    for ambiguous in [false, true] {
        let mut f = Fixture::new(
            "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
            context(LayoutPlatform::Win32),
        );
        f.unit(
            "Types",
            "unit Types; interface type Export=Int64; implementation end.",
            context(LayoutPlatform::Win32),
        );
        if ambiguous {
            f.ambiguous_unit("Types");
        } else {
            f.deny_unit("Types");
        }
        assert_eq!(
            activity(&f.analyze().unwrap(), "DEFINE HIT"),
            Truth::Unknown
        );
        assert!(f.read_names().is_empty());
    }
}

#[test]
fn provider_includes_inherit_provider_defines_and_layout_context() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$DEFINE PRIVATE} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface {$DEFINE PROVIDER}{$I types.inc} implementation end.",
        context(LayoutPlatform::Win32).with_absent_define(Truth::False),
    );
    f.include("types.inc","{$IFDEF PROVIDER} {$IFNDEF PRIVATE} type Export=NativeInt; {$ELSE} type Export=Byte; {$ENDIF} {$ENDIF}");
    // Separately compiled providers select their own platform too.
    f.unit(
        "Types",
        "unit Types; interface {$DEFINE PROVIDER}{$I types.inc} implementation end.",
        context(LayoutPlatform::Win64).with_absent_define(Truth::False),
    );
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn captured_constant_and_enum_array_bounds_use_shared_expression_language() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; const Limit=0; {$IF SizeOf(Types.Export)=24}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit("Types","unit Types; interface const Limit=(1 shl 1); type E=(First, Second, Third); Export=array[First..Third] of array[0..Limit-1] of Integer; implementation end.",context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE HIT"), Truth::True);
}

#[test]
fn independently_selected_same_named_exports_are_ambiguous() {
    let mut f = Fixture::new(
        "unit Main; interface uses A,B; {$IF SizeOf(Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    for name in ["A", "B"] {
        f.unit(
            name,
            &format!("unit {name}; interface type Export=Int64; implementation end."),
            context(LayoutPlatform::Win32),
        );
    }
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn qualified_alias_binding_respects_local_value_blocker() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; var Types:Byte; type Alias=Types.Export; {$IF SizeOf(Alias)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    assert_eq!(
        activity(&f.analyze().unwrap(), "DEFINE HIT"),
        Truth::Unknown
    );
}

#[test]
fn captured_interface_fingerprint_retains_physical_revision() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let first = f.analyze().unwrap().facts.fingerprint();
    f.unit_revision("Types", 2);
    assert_ne!(first, f.analyze().unwrap().facts.fingerprint());
}

#[test]
fn imported_layout_returns_provider_revision_dependency() {
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits, Fact, OccurrenceId};
    let mut f = Fixture::new(
        "unit Main; interface uses Types; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    let a = f.analyze().unwrap();
    let site = a
        .facts
        .site_at(&f.root.id, OccurrenceId(0), f.root.bytes.len())
        .unwrap();
    let mut budget =
        CompileTimeBudget::new(CompileTimeLimits::default(), &pascal_core::NoCancellation);
    let Fact::Known(layout) = a.facts.layout_name(site, "Types.Export", &mut budget) else {
        panic!("proven export layout");
    };
    assert!(layout.dependencies.iter().any(|observation|matches!(observation,pascal_core::ResolutionObservation::Payload { source_id,revision:pascal_core::SourceRevision::Overlay { version:1,.. },.. } if source_id.as_str()=="/fixture/Types.pas")));
}

#[test]
fn unused_active_import_does_not_compile_provider_interface() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface {$I unused.inc} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.include("unused.inc", "type Export=Int64;");
    f.analyze().unwrap();
    assert!(!f.read_names().contains(&"unused.inc".to_string()));
}

#[test]
fn declaration_limit_is_aggregate_across_root_and_provider_graphs() {
    let mut f = Fixture::new(
        "unit Main; interface uses Types; type Local=Byte; {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.unit(
        "Types",
        "unit Types; interface type Export=Int64; implementation end.",
        context(LayoutPlatform::Win32),
    );
    f.limits.max_definitions = 1;
    assert!(f.analyze().is_err());
}
