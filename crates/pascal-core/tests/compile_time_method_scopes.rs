//! l4d-wwmg: generic type declarations and method-body member scopes in
//! compile-time conditionals. dcc 37.0 ground truth (programs G1..G7, C1,
//! C2):
//! - a generic type `X<T>` does not declare the plain name `X`: it neither
//!   shadows `CompilerVersion` nor an earlier `const X`, `Declared(X)` is
//!   False for it alone, and `SizeOf(X)` names a plain `X`;
//! - the values of an enumerated type nested in a generic type are
//!   unit-scope names;
//! - in a method body a class constant of the class or of an ancestor
//!   shadows `CompilerVersion` (C1). A field does not (C2), but any member
//!   is treated as a possible binding here, which fails closed.
#[path = "support/compile_time.rs"]
mod support;
use pascal_core::conditional::Truth;
use pascal_project::LayoutPlatform;
use support::{Fixture, activity, context};

fn hit(source: &str) -> Truth {
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert!(a.reasons.is_empty(), "{:?}", a.reasons);
    activity(&a, "DEFINE HIT")
}

fn hit_with(source: &str, units: &[(&str, &str)]) -> Truth {
    let mut f = Fixture::new(source, context(LayoutPlatform::Win32));
    for (name, text) in units {
        f.unit(name, text, context(LayoutPlatform::Win32));
    }
    let a = f.analyze().unwrap();
    activity(&a, "DEFINE HIT")
}

/// A provider interface past `max_branch_states`, so its graph is unknown.
fn branch_limited(unit: &str, extra: &str) -> String {
    let mut text = format!("unit {unit}; interface ");
    for n in 0..7 {
        text.push_str(&format!("{{$IFDEF U{n}}} const C{n} = {n}; {{$ENDIF}} "));
    }
    text.push_str(extra);
    text.push_str(" implementation end.");
    text
}

const CV: &str = "{$IF CompilerVersion >= 19}{$DEFINE HIT}{$IFEND}";

fn method(declarations: &str, body: &str) -> String {
    format!(
        "unit T; interface {declarations} implementation procedure TC.M; begin {body} end; end."
    )
}

// Generic type declarations (SysUtils `TProc<T>`, OtlParallel
// `IOmniParallelLoop<T> = interface;`).

#[test]
fn a_generic_type_leaves_later_names_known() {
    for generic in [
        "TProc<T> = reference to procedure (Arg1: T);",
        "IOmniParallelLoop<T> = interface;",
        "TPair<K, V> = record Key: K; Value: V; end;",
        "TList<T: class> = class(TObject) FItems: array of T; end;",
    ] {
        assert_eq!(
            hit(&format!(
                "unit T; interface type {generic} {CV} implementation end."
            )),
            Truth::True,
            "{generic}"
        );
    }
}

#[test]
fn a_generic_type_does_not_declare_its_plain_name() {
    // dcc G2: `CompilerVersion<T>` does not shadow System's constant.
    assert_eq!(
        hit(&format!(
            "unit T; interface type CompilerVersion<T> = class end; {CV} implementation end."
        )),
        Truth::True
    );
    // dcc G3: nor an earlier constant of the same name.
    assert_eq!(
        hit("unit T; interface const X = 5; type X<T> = class end; \
             {$IF X = 5}{$DEFINE HIT}{$IFEND} implementation end."),
        Truth::True
    );
    // dcc G4: `SizeOf(TR)` is the plain type.
    assert_eq!(
        hit(
            "unit T; interface type TR<T> = record A, B, C: Int64; end; TR = Byte; \
             {$IF SizeOf(TR) = 1}{$DEFINE HIT}{$IFEND} implementation end."
        ),
        Truth::True
    );
    // dcc G1 says `Declared(TProc)` is False; it is at least not True.
    assert_ne!(
        hit(
            "unit T; interface type TProc<T> = reference to procedure (A: T); \
             {$IF Declared(TProc)}{$DEFINE HIT}{$IFEND} implementation end."
        ),
        Truth::True
    );
}

#[test]
fn nested_enum_values_of_a_generic_type_are_unit_scope_names() {
    // dcc G5/G7.
    assert_eq!(
        hit(
            "unit T; interface type TG<T> = record type TE = (geA, geB); end; \
             {$IF Ord(geB) = 1}{$DEFINE HIT}{$IFEND} implementation end."
        ),
        Truth::True
    );
    assert_eq!(
        hit(
            "unit T; interface type TG<T> = class type TE = (geA, geB); end; \
             {$IF Declared(geA)}{$DEFINE HIT}{$IFEND} implementation end."
        ),
        Truth::True
    );
}

#[test]
fn a_generic_method_body_is_not_proven_but_later_code_is() {
    let source = format!(
        "unit T; interface type TG<T> = class procedure M; end; implementation \
         procedure TG<T>.M; begin {{$IF CompilerVersion >= 19}}{{$DEFINE INSIDE}}{{$IFEND}} end; \
         {CV} end."
    );
    let a = Fixture::new(&source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE INSIDE"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}

// Method bodies: members of the class, its ancestors and helpers.

#[test]
fn a_method_of_a_class_without_the_member_is_known() {
    // The TStringBuilder shape: an implicit TObject ancestor, after a
    // generic type.
    let declarations = "type TProc<T> = reference to procedure (A: T); \
        TC = class private FData: array of Char; public procedure M; \
        function Append(const Value: TObject): TC; end;";
    assert_eq!(hit(&method(declarations, CV)), Truth::True);
    // An explicit TObject ancestor and a local ancestor.
    assert_eq!(
        hit(&method(
            "type TA = class(TObject) X: Integer; end; TC = class(TA) procedure M; end;",
            CV
        )),
        Truth::True
    );
    // A record method.
    assert_eq!(
        hit(&format!(
            "unit T; interface type TR = record A: Integer; procedure M; end; \
             implementation procedure TR.M; begin {CV} end; end."
        )),
        Truth::True
    );
}

#[test]
fn an_inherited_class_constant_shadows_compiler_version_in_a_method() {
    // dcc C1 takes TA's constant (1), so the condition is False there.
    let declarations =
        "type TA = class const CompilerVersion = 1; end; TC = class(TA) procedure M; end;";
    assert_eq!(hit(&method(declarations, CV)), Truth::Unknown);
}

#[test]
fn a_member_of_the_class_shadows_compiler_version_in_a_method() {
    for member in [
        "const CompilerVersion = 1;",
        "CompilerVersion: Integer;",
        "class var CompilerVersion: Integer;",
        "function CompilerVersion: Integer;",
        "property CompilerVersion: Integer read FX;",
        "type CompilerVersion = Byte;",
    ] {
        assert_eq!(
            hit(&method(
                &format!("type TC = class FX: Integer; {member} procedure M; end;"),
                CV
            )),
            Truth::Unknown,
            "{member}"
        );
    }
    assert_eq!(
        hit(&format!(
            "unit T; interface type TR = record const CompilerVersion = 1; procedure M; end; \
             implementation procedure TR.M; begin {CV} end; end."
        )),
        Truth::Unknown
    );
}

#[test]
fn a_tobject_member_shadows_a_unit_constant_in_a_method() {
    let declarations = "const Free = 1; type TC = class procedure M; end;";
    assert_eq!(
        hit(&method(declarations, "{$IF Free = 1}{$DEFINE HIT}{$IFEND}")),
        Truth::Unknown
    );
    // Outside the method the constant is proven.
    assert_eq!(
        hit(
            "unit T; interface const Free = 1; type TC = class procedure M; end; \
             {$IF Free = 1}{$DEFINE HIT}{$IFEND} implementation procedure TC.M; begin end; end."
        ),
        Truth::True
    );
}

#[test]
fn a_method_local_declaration_binds_before_members() {
    let declarations = "type TC = class const CompilerVersion = 1; procedure M; end;";
    let source = format!(
        "unit T; interface {declarations} implementation \
         procedure TC.M; const CompilerVersion = 30; begin \
         {{$IF CompilerVersion = 30}}{{$DEFINE HIT}}{{$IFEND}} end; end."
    );
    assert_eq!(hit(&source), Truth::True);
    // A parameter too.
    let source = "unit T; interface type TC = class const Free = 1; procedure M(Free: Byte); end; \
         implementation procedure TC.M(Free: Byte); begin \
         {$IF SizeOf(Free) = 1}{$DEFINE HIT}{$IFEND} end; end.";
    assert_eq!(hit(source), Truth::True);
}

#[test]
fn a_nested_routine_of_a_method_sees_the_members() {
    // dcc C1: TD.M.Inner takes System's constant; TC.M.Inner would take TA's.
    let source = "unit T; interface type TA = class const CompilerVersion = 1; end; \
        TC = class(TA) procedure M; end; TD = class procedure M; end; implementation \
        procedure TC.M; procedure Inner; begin {$IF CompilerVersion >= 19}{$DEFINE INNER_C}{$IFEND} end; begin end; \
        procedure TD.M; procedure Inner; begin {$IF CompilerVersion >= 19}{$DEFINE INNER_D}{$IFEND} end; begin end; \
        end.";
    let a = Fixture::new(source, context(LayoutPlatform::Win32))
        .analyze()
        .unwrap();
    assert_eq!(activity(&a, "DEFINE INNER_C"), Truth::Unknown);
    assert_eq!(activity(&a, "DEFINE INNER_D"), Truth::True);
}

#[test]
fn a_class_helper_member_shadows_in_a_method() {
    let declarations = "type TC = class procedure M; end; \
        TCHelper = class helper for TC const CompilerVersion = 1; end;";
    assert_eq!(hit(&method(declarations, CV)), Truth::Unknown);
    let imported = "unit H; interface type TCHelper = class helper for TObject \
        const CompilerVersion = 1; end; implementation end.";
    assert_eq!(
        hit_with(
            &method("uses H; type TC = class procedure M; end;", CV),
            &[("H", imported)]
        ),
        Truth::Unknown
    );
}

#[test]
fn an_unknown_interface_that_never_spells_helper_declares_no_helper_member() {
    let provider = branch_limited("H", "type TOther = class procedure Foo; end;");
    assert_eq!(
        hit_with(
            &method("uses H; type TC = class procedure M; end;", CV),
            &[("H", &provider)]
        ),
        Truth::True
    );
    // A helper there, even one that spells neither the name nor an
    // ancestor, keeps it unknown (fix round 1).
    for helper in [
        "type TCHelper = class helper for TObject procedure Foo; end;",
        "type TCHelper = class helper for TObject const CompilerVersion = 1; end;",
        "type TCHelper = record HELPER for Integer end;",
        "{$I h.inc}",
    ] {
        let provider = branch_limited("H", helper);
        let mut f = Fixture::new(
            &method("uses H; type TC = class procedure M; end;", CV),
            context(LayoutPlatform::Win32),
        );
        f.unit("H", &provider, context(LayoutPlatform::Win32));
        f.include("h.inc", "type TIncHelper = class helper for TObject end;");
        let a = f.analyze().unwrap();
        assert_eq!(activity(&a, "DEFINE HIT"), Truth::Unknown, "{helper}");
    }
}

/// Review P1 (dcc 37.0: False): a helper inherits its ancestor helper's
/// members from another unit.
#[test]
fn a_helper_inherits_its_ancestor_helpers_members() {
    let base = "unit HB; interface type TBaseH = class helper for TObject \
        const CompilerVersion = 1; end; implementation end.";
    let consumer = method("uses HD; type TC = class procedure M; end;", CV);
    // HD unknown.
    let derived = branch_limited(
        "HD",
        "uses HB; type TDerivH = class helper(TBaseH) for TObject procedure Foo; end;",
    );
    assert_eq!(
        hit_with(&consumer, &[("HD", &derived), ("HB", base)]),
        Truth::Unknown
    );
    // HD known: the ancestor is followed into HB, which declares it.
    let derived = "unit HD; interface uses HB; type TDerivH = class helper(TBaseH) \
        for TObject procedure Foo; end; implementation end.";
    assert_eq!(
        hit_with(&consumer, &[("HD", derived), ("HB", base)]),
        Truth::Unknown
    );
    // A local helper whose ancestor is imported.
    let local = method(
        "uses HB; type TC = class procedure M; end; \
         TLocalH = class helper(TBaseH) for TObject end;",
        CV,
    );
    assert_eq!(hit_with(&local, &[("HB", base)]), Truth::Unknown);
    // An ancestor helper without the name, in a known unit, proves it.
    let base = "unit HB; interface type TBaseH = class helper for TObject \
        procedure Bar; end; implementation end.";
    assert_eq!(hit_with(&local, &[("HB", base)]), Truth::True);
}

#[test]
fn an_imported_ancestor_is_read_from_its_provider() {
    let base = "unit Base; interface type TBase = class procedure Foo; end; implementation end.";
    let consumer = method("uses Base; type TC = class(TBase) procedure M; end;", CV);
    assert_eq!(hit_with(&consumer, &[("Base", base)]), Truth::True);
    let base = "unit Base; interface type TBase = class const CompilerVersion = 1; end; \
        implementation end.";
    assert_eq!(hit_with(&consumer, &[("Base", base)]), Truth::Unknown);
    // Two levels, across a second provider.
    let base = "unit Base; interface uses Deep; type TBase = class(TDeep) procedure Foo; end; \
        implementation end.";
    let deep = "unit Deep; interface type TDeep = class const CompilerVersion = 1; end; \
        implementation end.";
    assert_eq!(
        hit_with(&consumer, &[("Base", base), ("Deep", deep)]),
        Truth::Unknown
    );
    let deep = "unit Deep; interface type TDeep = class X: Integer; end; implementation end.";
    assert_eq!(
        hit_with(&consumer, &[("Base", base), ("Deep", deep)]),
        Truth::True
    );
}

#[test]
fn an_ancestor_from_an_unknown_interface_is_not_proven() {
    // The ancestor's own interface never spells CompilerVersion, but an
    // unknown interface cannot prove the ancestor's own ancestry: here the
    // heritage list is conditional, and TDeep's unit is not read.
    let base = branch_limited(
        "Base",
        "uses Deep; type TBase = class{$IFDEF U9}(TDeep){$ENDIF} procedure Foo; end;",
    );
    let deep = "unit Deep; interface type TDeep = class const CompilerVersion = 1; end; \
        implementation end.";
    let consumer = method("uses Base; type TC = class(TBase) procedure M; end;", CV);
    assert_eq!(
        hit_with(&consumer, &[("Base", &base), ("Deep", deep)]),
        Truth::Unknown
    );
}

#[test]
fn an_unresolved_ancestor_stays_unknown() {
    assert_eq!(
        hit(&method("type TC = class(TMissing) procedure M; end;", CV)),
        Truth::Unknown
    );
    // A forward declaration alone is not the class.
    assert_eq!(
        hit(&format!(
            "unit T; interface type TC = class; implementation procedure TC.M; begin {CV} end; end."
        )),
        Truth::Unknown
    );
}

#[test]
fn an_implemented_interface_adds_no_member() {
    // dcc C3: an interface property does not shadow System's constant.
    let declarations = "type IFoo = interface function GetCV: Integer; \
        property CompilerVersion: Integer read GetCV; end; \
        TA = class end; TC = class(TA, IFoo) function GetCV: Integer; procedure M; end;";
    assert_eq!(hit(&method(declarations, CV)), Truth::True);
}

#[test]
fn types_a_member_declaration_refers_to_are_not_members() {
    let declarations = "type TC = class FCount: Integer; function Get(Index: Integer): Byte; \
        procedure M; end;";
    assert_eq!(
        hit(&method(
            declarations,
            "{$IF (SizeOf(Integer) = 4) and (SizeOf(Byte) = 1)}{$DEFINE HIT}{$IFEND}"
        )),
        Truth::True
    );
}

// Scanner framing the fix above exposed in SysUtils and OtlParallel: each
// used to end every later proof.

#[test]
fn a_class_nested_in_a_class_body_keeps_the_framing() {
    // OtlParallel's TOmniParallelLoop<T>: a nested class type.
    for nested in [
        "TInner = class(TObject) private F: Integer; end;",
        "TInner = class F: Integer; end;",
        "TInner = class;",
        "TInner = class(TObject);",
        "TMeta = class of TObject;",
        "TInner = class sealed F: Integer; end;",
    ] {
        let source = format!(
            "unit T; interface type TOuter = class strict private type {nested} \
             var G: Integer; public procedure M; end; TNext = class X: Integer; end; \
             {CV} implementation end."
        );
        assert_eq!(hit(&source), Truth::True, "{nested}");
    }
}

#[test]
fn a_calling_convention_after_a_procedural_type_keeps_the_framing() {
    // SysUtils' GetFileAttributesExFunc.
    for declaration in [
        "function Pre(A: Integer): Boolean; stdcall; forward; \
         var F: function(A: Integer): Boolean; stdcall = Pre;",
        "var F: procedure(A: Integer); cdecl;",
        "type TF = function: Integer; stdcall;",
        "type TM = procedure of object; register;",
        "var G: procedure; safecall; varargs;",
    ] {
        let source = format!("unit T; interface implementation {declaration} {CV} end.");
        assert_eq!(hit(&source), Truth::True, "{declaration}");
    }
    // Anything else after the procedural type is still not admitted.
    assert_eq!(
        hit(&format!(
            "unit T; interface implementation var F: procedure; stdcall X = 1; {CV} end."
        )),
        Truth::Unknown
    );
}

/// Review minor 1: the calling-convention skip applies only right after
/// the procedural declaration, in the same section.
#[test]
fn a_calling_convention_named_declaration_in_a_new_section_is_kept() {
    for (declaration, condition) in [
        ("var F: procedure; const stdcall = 1;", "stdcall = 1"),
        ("var F: procedure; const stdcall = 1;", "Declared(stdcall)"),
        ("type TF = procedure; const cdecl = 2;", "cdecl = 2"),
        (
            "var F: procedure; X: Integer; const stdcall = 1;",
            "stdcall = 1",
        ),
    ] {
        let source = format!(
            "unit T; interface {declaration} {{$IF {condition}}}{{$DEFINE HIT}}{{$IFEND}} implementation end."
        );
        assert_eq!(hit(&source), Truth::True, "{declaration} {condition}");
    }
}
