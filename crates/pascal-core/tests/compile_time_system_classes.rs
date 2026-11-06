//! l4d-rrzi: Log.pas (`TLogFile = class(TObject, IInterface, ...)`) and the
//! Delphi 2010 Classes.pas interface it imports.
//! - A `var`/typed `const` of a method-pointer type with an initialiser
//!   (`AddDataModule: procedure (...) of object = nil;`) left the declaration
//!   scanner inside a composite, so Classes's interface never completed.
//! - In a class's heritage list only the first entry can be the ancestor
//!   class; implemented interfaces add no member to a method's scope (dcc
//!   C3), whether they resolve or not.
//! - System's IInterface/IUnknown/IDispatch/IInvokable are interfaces, and
//!   TInterfacedObject/TAggregatedObject/TContainedObject have fixed member
//!   lists, as TObject has.
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

const CV: &str = "{$IF CompilerVersion >= 19}{$DEFINE HIT}{$IFEND}";

fn method(declarations: &str, body: &str) -> String {
    format!(
        "unit T; interface {declarations} implementation procedure TC.M; begin {body} end; end."
    )
}

#[test]
fn a_method_pointer_variable_with_an_initialiser_keeps_the_framing() {
    for declaration in [
        "var P: procedure of object = nil;",
        "var P: procedure (S: TObject) of object = nil; Q: function: Integer of object = nil;",
        "const P: procedure of object = nil;",
    ] {
        assert_eq!(
            hit(&format!(
                "unit T; interface {declaration} const X = 1; \
                 {{$IF X = 1}}{{$DEFINE HIT}}{{$IFEND}} implementation end."
            )),
            Truth::True,
            "{declaration}"
        );
    }
}

#[test]
fn a_provider_with_method_pointer_variables_has_a_complete_interface() {
    // The Classes.pas 7.0 shape (lines 1599-1602, then routine headings
    // with default parameters before `implementation`).
    let classes = "unit Classes; interface type TDataModule = class end; \
        var AddDataModule: procedure (DataModule: TDataModule) of object = nil; \
        RemoveDataModule: procedure (DataModule: TDataModule) of object = nil; \
        function IsDefault(Instance: TObject; Writer: TObject = nil): Boolean; \
        const X = 1; implementation end.";
    assert_eq!(
        hit_with(
            "unit T; interface uses Classes; {$IF X = 1}{$DEFINE HIT}{$IFEND} implementation end.",
            &[("Classes", classes)]
        ),
        Truth::True
    );
    // The Log.pas shape: a method body of a class whose ancestry the
    // provider's interface must prove.
    assert_eq!(
        hit_with(
            &method(
                "uses Classes; type TC = class(TObject, IInterface) procedure M; end;",
                CV
            ),
            &[("Classes", classes)]
        ),
        Truth::True
    );
}

#[test]
fn an_implemented_system_interface_does_not_block_a_method() {
    for heritage in [
        "TObject, IInterface",
        "TObject, IUnknown, IDispatch",
        "IInterface",
        "IInvokable, IMissing",
        "TObject, IInterface, ILocal, IMissing",
    ] {
        assert_eq!(
            hit(&method(
                &format!(
                    "type ILocal = interface procedure Foo; end; \
                     TC = class({heritage}) procedure M; end;"
                ),
                CV
            )),
            Truth::True,
            "{heritage}"
        );
    }
    // A class whose first entry is a System interface derives from TObject.
    assert_eq!(
        hit(&method(
            "const Free = 1; type TC = class(IInterface) procedure M; end;",
            "{$IF Free = 1}{$DEFINE HIT}{$IFEND}"
        )),
        Truth::Unknown
    );
    // An unresolved first entry is still unproven.
    assert_eq!(
        hit(&method(
            "type TC = class(TMissing, IInterface) procedure M; end;",
            CV
        )),
        Truth::Unknown
    );
}

#[test]
fn a_system_interfaced_class_ancestor_is_known() {
    for ancestor in ["TInterfacedObject", "TAggregatedObject", "TContainedObject"] {
        assert_eq!(
            hit(&method(
                &format!("type TC = class({ancestor}, IInterface) procedure M; end;"),
                CV
            )),
            Truth::True,
            "{ancestor}"
        );
    }
    // Through a local class.
    assert_eq!(
        hit(&method(
            "type TA = class(TInterfacedObject) end; TC = class(TA) procedure M; end;",
            CV
        )),
        Truth::True
    );
}

#[test]
fn a_member_of_a_system_interfaced_class_blocks_a_method() {
    for (ancestor, member) in [
        ("TInterfacedObject", "QueryInterface"),
        ("TInterfacedObject", "_Release"),
        ("TInterfacedObject", "GetRefCount"),
        ("TInterfacedObject", "Free"),
        ("TAggregatedObject", "Controller"),
        ("TAggregatedObject", "FController"),
        ("TContainedObject", "Controller"),
        ("TContainedObject", "_AddRef"),
        ("TContainedObject", "ClassName"),
    ] {
        assert_eq!(
            hit(&method(
                &format!("const {member} = 1; type TC = class({ancestor}) procedure M; end;"),
                &format!("{{$IF {member} = 1}}{{$DEFINE HIT}}{{$IFEND}}")
            )),
            Truth::Unknown,
            "{ancestor}.{member}"
        );
    }
    // Not a TObject member: a class without that ancestor proves it.
    assert_eq!(
        hit(&method(
            "const QueryInterface = 1; type TC = class(TObject, IInterface) procedure M; end;",
            "{$IF QueryInterface = 1}{$DEFINE HIT}{$IFEND}"
        )),
        Truth::True
    );
    // A local class of that name is read instead of System's.
    assert_eq!(
        hit(&method(
            "const QueryInterface = 1; type TInterfacedObject = class end; \
             TC = class(TInterfacedObject) procedure M; end;",
            "{$IF QueryInterface = 1}{$DEFINE HIT}{$IFEND}"
        )),
        Truth::True
    );
}
