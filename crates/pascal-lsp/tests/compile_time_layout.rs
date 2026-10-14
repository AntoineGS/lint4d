use lsp_types::{Location, Position, Range, Url};
use pascal_lsp::NavigationTarget;
use pascal_lsp::workspace::{Workspace, WorkspaceOptions};
use pascal_project::CompilerVersion;
use pascal_project::delphi_overrides::OverrideSession;
use std::{fs, path::Path};

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn workspace(root: &Path, platform: &str) -> Workspace {
    let mut options = WorkspaceOptions {
        platform: Some(platform.into()),
        ..WorkspaceOptions::default()
    };
    options.conditional_context.compiler_version = Some(CompilerVersion::new(37, 0));
    Workspace::with_override_session(vec![root.into()], options, OverrideSession::new(None))
}

fn position(source: &str, word: &str) -> Position {
    let offset = source.find(word).unwrap();
    let prefix = &source[..offset];
    Position::new(
        prefix.bytes().filter(|b| *b == b'\n').count() as u32,
        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
    )
}

fn header(root: &Path, file: &str, source: &str, name: &str) -> Location {
    let start = position(source, name);
    Location::new(
        Url::from_file_path(root.join(file)).unwrap(),
        Range::new(
            start,
            Position::new(
                start.line,
                start.character + name.encode_utf16().count() as u32,
            ),
        ),
    )
}

const MAIN: &str = "unit Main;\ninterface\nuses PreambleUnit, SysUtils, Base;\n{$IF SizeOf(Extended) >= 10}\nuses WideProvider;\n{$ELSE}\nuses NarrowProvider;\n{$ENDIF}\nimplementation\nend.\n";
const PREAMBLE: &str = "{$IF Defined(IOS) OR Defined(ANDROID)} {$HPPEMIT LINKUNIT}{$ELSE}{$HPPEMIT '#pragma link unit'}{$ENDIF}\nunit PreambleUnit; interface implementation end.";
const SYSUTILS: &str = "unit System.SysUtils; interface\n{$IF SizeOf(Extended) >= 10}\ntype Selected = Extended;\n{$ELSE}\ntype Selected = Double;\n{$ENDIF}\nimplementation end.";
const BASE: &str = "unit Base; interface const Revision={$I revision.inc}; {$IF Revision=7}{$I chosen.inc}{$ELSE}{$I forbidden.inc}{$ENDIF} implementation end.";

#[test]
fn task13_workspace_sizeof_whitespace_and_trivia_select_real_branches() {
    for operand in [
        "SizeOf ( Types.T )",
        "SizeOf { trivia } (LongWord)",
        "SizeOf (* ) , trivia *) (LongWord)",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture(root);
        let source = format!(
            "unit Main; interface uses Types; {{$IF {operand}=4}} uses Small; {{$ELSE}} uses Large; {{$ENDIF}} implementation end."
        );
        write(&root.join("Main.pas"), &source);
        write(
            &root.join("Types.pas"),
            "unit Types; interface type T=LongWord; implementation end.",
        );
        let small = "unit Small; interface implementation end.";
        write(&root.join("Small.pas"), small);
        write(
            &root.join("Large.pas"),
            "unit Large; interface implementation end.",
        );
        let main = Url::from_file_path(root.join("Main.pas")).unwrap();
        let mut workspace = workspace(root, "Win32");
        for _ in 0..2 {
            assert_eq!(
                workspace.navigate(
                    &main,
                    position(&source, "Small"),
                    NavigationTarget::Definition
                ),
                vec![header(root, "Small.pas", small, "Small")],
                "{operand}"
            );
            assert!(
                workspace
                    .navigate(
                        &main,
                        position(&source, "Large"),
                        NavigationTarget::Definition
                    )
                    .is_empty()
            );
        }
    }
}

#[test]
fn task13_fix1_workspace_malformed_newline_literal_cannot_select_precise_import() {
    for newline in ["\n", "\r\n", "\r"] {
        for split in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            fixture(root);
            let opening = if split { "{$I head.inc}" } else { "{$IF 'a}" };
            let source = format!(
                "unit Main; interface {opening}{newline}b'='a}}{newline}b'}} uses Small; {{$ELSE}} uses Large; {{$ENDIF}} implementation end."
            );
            write(&root.join("Main.pas"), &source);
            write(&root.join("head.inc"), "{$IF 'a}");
            for name in ["Small", "Large"] {
                write(
                    &root.join(format!("{name}.pas")),
                    &format!("unit {name}; interface implementation end."),
                );
            }
            let main = Url::from_file_path(root.join("Main.pas")).unwrap();
            let mut workspace = workspace(root, "Win32");
            for _ in 0..2 {
                for name in ["Small", "Large"] {
                    assert!(
                        workspace
                            .navigate(&main, position(&source, name), NavigationTarget::Definition)
                            .is_empty(),
                        "newline={newline:?} split={split} name={name}"
                    );
                }
            }
        }
    }
}

fn task13_assert_branch(
    workspace: &mut Workspace,
    root: &Path,
    source: &str,
    selected: &str,
    inactive: &str,
) {
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let selected_source = format!("unit {selected}; interface implementation end.");
    for _ in 0..2 {
        assert_eq!(
            workspace.navigate(
                &main,
                position(source, selected),
                NavigationTarget::Definition
            ),
            vec![header(
                root,
                &format!("{selected}.pas"),
                &selected_source,
                selected
            )]
        );
        assert!(
            workspace
                .navigate(
                    &main,
                    position(source, inactive),
                    NavigationTarget::Definition
                )
                .is_empty()
        );
    }
}

#[test]
fn task13_provider_overlay_include_packing_and_structural_alias_edits_are_fresh() {
    use pascal_lsp::workspace::FileChange;
    for (before, after, size, include_edit) in [
        ("type Payload=Byte;", "type Payload=Int64;", 1, false),
        (
            "const Bound={$I bound.inc}; type Payload=array[1..Bound] of Byte;",
            "const Bound={$I bound.inc}; type Payload=array[1..Bound] of Byte;",
            1,
            true,
        ),
        (
            "{$A1} type Payload=record B:Byte; N:LongWord; end;",
            "{$A8} type Payload=record B:Byte; N:LongWord; end;",
            5,
            false,
        ),
        (
            "{$A8} type Inner=record B:Byte; end; Payload=Inner;",
            "{$A8} type Inner=record B:Int64; end; Payload=Inner;",
            1,
            false,
        ),
    ] {
        eprintln!("Task13 freshness case: {before}");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture(root);
        let source = format!(
            "unit Main; interface uses Types; {{$IF SizeOf(Types.Payload)={size}}} uses Small; {{$ELSE}} uses Large; {{$ENDIF}} implementation end."
        );
        write(&root.join("Main.pas"), &source);
        write(&root.join("bound.inc"), "1");
        let provider = |body: &str| format!("unit Types; interface {body} implementation end.");
        write(&root.join("Types.pas"), &provider(before));
        for name in ["Small", "Large"] {
            write(
                &root.join(format!("{name}.pas")),
                &format!("unit {name}; interface implementation end."),
            );
        }
        let mut workspace = workspace(root, "Win32");
        task13_assert_branch(&mut workspace, root, &source, "Small", "Large");
        if include_edit {
            write(&root.join("bound.inc"), "8");
            workspace.file_event(
                &Url::from_file_path(root.join("bound.inc")).unwrap(),
                FileChange::Changed,
            );
        } else {
            let uri = Url::from_file_path(root.join("Types.pas")).unwrap();
            workspace
                .open_document(uri.clone(), provider(before), 1)
                .unwrap();
            task13_assert_branch(&mut workspace, root, &source, "Small", "Large");
            workspace.change_document(uri, provider(after), 2).unwrap();
        }
        task13_assert_branch(&mut workspace, root, &source, "Large", "Small");
    }
}

#[test]
fn task13_target_selection_revalidates_imported_native_width() {
    use pascal_lsp::workspace::projects::BuildField;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    write(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><Platform>Win32</Platform></PropertyGroup><ProjectExtensions><BorlandProject><Platforms><Platform value=\"Win32\">True</Platform><Platform value=\"Win64\">True</Platform></Platforms></BorlandProject></ProjectExtensions></Project>",
    );
    let source = "unit Main; interface uses Types; {$IF SizeOf(Types.Payload)=4} uses Small; {$ELSE} uses Large; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), source);
    write(
        &root.join("Types.pas"),
        "unit Types; interface type Payload=NativeInt; implementation end.",
    );
    for name in ["Small", "Large"] {
        write(
            &root.join(format!("{name}.pas")),
            &format!("unit {name}; interface implementation end."),
        );
    }
    let mut workspace = workspace(root, "Win32");
    let project = Url::from_file_path(root.join("App.dproj")).unwrap();
    task13_assert_branch(&mut workspace, root, source, "Small", "Large");
    workspace
        .select_build(&project, BuildField::Platform, Some("Win64"))
        .unwrap();
    task13_assert_branch(&mut workspace, root, source, "Large", "Small");
    workspace
        .select_build(&project, BuildField::Platform, Some("Win32"))
        .unwrap();
    task13_assert_branch(&mut workspace, root, source, "Small", "Large");
}

#[test]
fn task13_workspace_trivia_operands_remain_fail_closed_under_shadowing() {
    for (declaration, operand) in [
        ("var Types:Byte;", "SizeOf ( Types.T )"),
        (
            "function SizeOf(X:LongWord):Integer;",
            "SizeOf { trivia } (LongWord)",
        ),
        ("", "SizeOf { trivia } (Missing)"),
        ("", "SizeOf { trivia } (LongWord,Byte)"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture(root);
        let source = format!(
            "unit Main;\ninterface uses Types;\n{{ é😀 }} {declaration}\n{{$IF {operand}=4}} uses Small; {{$ELSE}} uses Large; {{$ENDIF}} implementation end."
        );
        write(&root.join("Main.pas"), &source);
        write(
            &root.join("Types.pas"),
            "unit Types; interface type T=LongWord; implementation end.",
        );
        for name in ["Small", "Large"] {
            write(
                &root.join(format!("{name}.pas")),
                &format!("unit {name}; interface implementation end."),
            );
        }
        let main = Url::from_file_path(root.join("Main.pas")).unwrap();
        let mut workspace = workspace(root, "Win32");
        for _ in 0..2 {
            for name in ["Small", "Large"] {
                assert!(
                    workspace
                        .navigate(&main, position(&source, name), NavigationTarget::Definition)
                        .is_empty(),
                    "{declaration}: {operand}: {name}"
                );
            }
        }
    }
}

fn nested_constant_audit_rename(roots: &[(&str, i32)]) -> Result<lsp_types::WorkspaceEdit, String> {
    let temp = tempfile::tempdir().unwrap();
    for (name, n) in roots {
        write(
            &temp.path().join(format!("{name}.pas")),
            &format!("unit {name}; interface const N={n}; {{$I Outer.inc}} implementation end."),
        );
    }
    write(&temp.path().join("Outer.inc"), "{$I Child.inc}");
    write(
        &temp.path().join("Child.inc"),
        "{$IF N=1} const BadConst=1; {$ENDIF}",
    );
    write(
        &temp.path().join("Main.pas"),
        "unit Main; interface const BadConst=1; implementation end.",
    );
    let uri = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
    let mut workspace = Workspace::with_override_session(
        vec![temp.path().into()],
        WorkspaceOptions::default(),
        OverrideSession::new(None),
    );
    workspace.rename_edits(&uri, Position::new(0, 27), "GoodConst", true)
}

#[test]
fn nested_include_audit_refuses_unsafe_second_root_despite_safe_first_session() {
    let error = nested_constant_audit_rename(&[("A", 0), ("B", 1)]).unwrap_err();
    assert!(
        error.contains("Child.inc")
            && error.contains("conditional compilation affecting the rename"),
        "{error}"
    );
}

#[test]
fn nested_include_audit_single_unsafe_root_and_reversed_entry_facts_still_refuse() {
    for roots in [&[("B", 1)][..], &[("A", 1), ("B", 0)][..]] {
        let error = nested_constant_audit_rename(roots).unwrap_err();
        assert!(
            error.contains("Child.inc")
                && error.contains("conditional compilation affecting the rename"),
            "{error}"
        );
    }
}

#[test]
fn nested_include_audit_different_safe_entry_facts_remain_accepted() {
    assert!(nested_constant_audit_rename(&[("A", 0), ("B", 2)]).is_ok());
}

#[test]
fn default_include_depth_boundary_preserves_source_order_and_physical_definition() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface {$I chain0.inc} implementation procedure Run; begin DeepValue := 1; end; end.";
    write(&root.join("Main.pas"), source);
    for depth in 0..64 {
        let text = if depth == 63 {
            "const DeepValue = 1;".into()
        } else {
            format!("{{$I chain{}.inc}}", depth + 1)
        };
        write(&root.join(format!("chain{depth}.inc")), &text);
    }
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "DeepValue"),
            NavigationTarget::Definition
        ),
        vec![header(
            root,
            "chain63.inc",
            "const DeepValue = 1;",
            "DeepValue"
        )],
    );
}

fn fixture(root: &Path) -> Url {
    write(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_UnitAlias>SysUtils=System.SysUtils</DCC_UnitAlias></PropertyGroup></Project>",
    );
    for (file, text) in [
        ("Main.pas", MAIN),
        ("PreambleUnit.pas", PREAMBLE),
        ("System.SysUtils.pas", SYSUTILS),
        ("Base.pas", BASE),
        ("revision.inc", "7"),
        ("chosen.inc", "type Small=array[1..Revision] of Byte;"),
        (
            "WideProvider.pas",
            "unit WideProvider; interface implementation end.",
        ),
        (
            "NarrowProvider.pas",
            "unit NarrowProvider; interface implementation end.",
        ),
    ] {
        write(&root.join(file), text);
    }
    Url::from_file_path(root.join("Main.pas")).unwrap()
}

#[test]
fn generated_workspace_imports_and_target_branches_have_physical_definitions() {
    for (platform, selected, inactive) in [
        ("Win32", "WideProvider", "NarrowProvider"),
        ("Win64", "NarrowProvider", "WideProvider"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let main = fixture(root);
        let mut workspace = workspace(root, platform);
        for (query, file, text, name) in [
            ("PreambleUnit", "PreambleUnit.pas", PREAMBLE, "PreambleUnit"),
            (
                "SysUtils",
                "System.SysUtils.pas",
                SYSUTILS,
                "System.SysUtils",
            ),
            ("Base", "Base.pas", BASE, "Base"),
        ] {
            assert_eq!(
                workspace.navigate(&main, position(MAIN, query), NavigationTarget::Definition),
                vec![header(root, file, text, name)],
                "{platform}: {query}"
            );
        }
        let file = format!("{selected}.pas");
        let source = fs::read_to_string(root.join(&file)).unwrap();
        assert_eq!(
            workspace.navigate(
                &main,
                position(MAIN, selected),
                NavigationTarget::Definition
            ),
            vec![header(root, &file, &source, selected)],
            "{platform}: selected branch"
        );
        assert!(
            workspace
                .navigate(
                    &main,
                    position(MAIN, inactive),
                    NavigationTarget::Definition
                )
                .is_empty(),
            "{platform}: inactive branch"
        );
    }
}

#[test]
fn unsupported_body_preserves_only_known_header_import_definition() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main_source = "unit Main; interface uses Known, Unknown; implementation end.";
    let known = "unit Known; interface {$IF UnsupportedFunction(1)} const Value=1; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), main_source);
    write(&root.join("Known.pas"), known);
    write(
        &root.join("Unknown.pas"),
        "{$IF UnsupportedFunction(1)} unit Unknown; {$ENDIF} interface implementation end.",
    );
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert_eq!(
        workspace.navigate(
            &main,
            position(main_source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", known, "Known")]
    );
    assert!(
        workspace
            .navigate(
                &main,
                position(main_source, "Unknown"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
}

#[test]
fn rf_decoded_header_map() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main_source = "unit Main; interface uses Known; implementation end.";
    let known = "{é 🦀 prefix}\nunit Known; interface {$IF UnsupportedFunction(1)} const Value=1; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), main_source);
    let mut bytes = vec![0xff, 0xfe];
    for unit in known.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    fs::write(root.join("Known.pas"), bytes).unwrap();
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    let expected = vec![header(root, "Known.pas", known, "Known")];
    assert_eq!(
        workspace.navigate(
            &main,
            position(main_source, "Known"),
            NavigationTarget::Definition
        ),
        expected
    );
    workspace
        .open_document(
            Url::from_file_path(root.join("Known.pas")).unwrap(),
            known.into(),
            1,
        )
        .unwrap();
    assert_eq!(
        workspace.navigate(
            &main,
            position(main_source, "Known"),
            NavigationTarget::Definition
        ),
        expected
    );
}

#[test]
fn included_initializer_and_imported_provider_facts_survive_warm_navigation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let main_source = "unit Main; interface uses Base; {$IF SizeOf(Base.Small)=7} uses WideProvider; {$ELSE} uses NarrowProvider; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), main_source);
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    let wide = "unit WideProvider; interface implementation end.";
    for _ in 0..2 {
        assert_eq!(
            workspace.navigate(
                &main,
                position(main_source, "WideProvider"),
                NavigationTarget::Definition
            ),
            vec![header(root, "WideProvider.pas", wide, "WideProvider")]
        );
        assert!(
            workspace
                .navigate(
                    &main,
                    position(main_source, "NarrowProvider"),
                    NavigationTarget::Definition
                )
                .is_empty()
        );
    }
    workspace
        .open_document(
            Url::from_file_path(root.join("chosen.inc")).unwrap(),
            "type Small=array[1..8] of Byte;".into(),
            1,
        )
        .unwrap();
    assert!(
        workspace
            .navigate(
                &main,
                position(main_source, "WideProvider"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
    let narrow = "unit NarrowProvider; interface implementation end.";
    assert_eq!(
        workspace.navigate(
            &main,
            position(main_source, "NarrowProvider"),
            NavigationTarget::Definition
        ),
        vec![header(root, "NarrowProvider.pas", narrow, "NarrowProvider")]
    );
}

#[test]
fn header_fallback_never_grants_body_or_rename_precision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface uses Known; implementation procedure Run; begin Value := 1; end; end.";
    let known = "unit Known; interface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), source);
    write(&root.join("Known.pas"), known);
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", known, "Known")]
    );
    assert!(
        workspace
            .navigate(
                &main,
                position(source, "Value"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
    let error = workspace
        .rename_edits(&main, position(source, "Known"), "Renamed", true)
        .unwrap_err();
    assert!(
        error.contains("incomplete") || error.contains("conditional"),
        "{error}"
    );
}

#[test]
fn verified_header_prefix_dependencies_and_coordinates_are_fresh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface uses Known; implementation end.";
    let known = "{$I flags.inc}\n{$IF Defined(READY)}\nunit Known;\n{$ELSE}\nunit Other;\n{$ENDIF}\ninterface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), source);
    write(&root.join("Known.pas"), known);
    write(&root.join("flags.inc"), "{$DEFINE READY}");
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", known, "Known")]
    );
    write(&root.join("flags.inc"), "{$UNDEF READY}");
    workspace.file_event(
        &Url::from_file_path(root.join("flags.inc")).unwrap(),
        pascal_lsp::workspace::FileChange::Changed,
    );
    assert!(
        workspace
            .navigate(
                &main,
                position(source, "Known"),
                NavigationTarget::Definition
            )
            .is_empty(),
        "a selected source whose header now differs cannot reuse the old proof"
    );
    workspace
        .open_document(
            Url::from_file_path(root.join("flags.inc")).unwrap(),
            "{$DEFINE READY}".into(),
            1,
        )
        .unwrap();
    let shifted = format!("{{é 🦀 shifted}}\n{known}");
    workspace
        .open_document(
            Url::from_file_path(root.join("Known.pas")).unwrap(),
            shifted.clone(),
            1,
        )
        .unwrap();
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", &shifted, "Known")]
    );
    workspace
        .change_document(
            Url::from_file_path(root.join("flags.inc")).unwrap(),
            "{$UNDEF READY}".into(),
            2,
        )
        .unwrap();
    assert!(
        workspace
            .navigate(
                &main,
                position(source, "Known"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
    workspace
        .change_document(
            Url::from_file_path(root.join("flags.inc")).unwrap(),
            "{$DEFINE READY}".into(),
            3,
        )
        .unwrap();
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", &shifted, "Known")]
    );
}

#[test]
fn include_spanning_header_has_no_guessed_editable_name_range() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface uses Prefix.Known; implementation end.";
    write(&root.join("Main.pas"), source);
    write(
        &root.join("Prefix.Known.pas"),
        "unit Prefix.{$I suffix.inc}; interface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.",
    );
    write(&root.join("suffix.inc"), "Known");
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert!(
        workspace
            .navigate(
                &main,
                position(source, "Known"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
    let error = workspace
        .rename_edits(&main, position(source, "Known"), "Changed", true)
        .unwrap_err();
    assert!(
        error.contains("incomplete") || error.contains("include") || error.contains("conditional"),
        "{error}"
    );
}

#[test]
fn ambiguous_header_and_declared_name_mismatch_do_not_use_filename_fallbacks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface uses Ambiguous, Mismatch; implementation end.";
    write(&root.join("Main.pas"), source);
    write(
        &root.join("Ambiguous.pas"),
        "{$IF UnsupportedFunction(1)} unit Ambiguous; {$ELSE} unit Other; {$ENDIF} interface implementation end.",
    );
    write(
        &root.join("Mismatch.pas"),
        "unit Different; interface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.",
    );
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    for name in ["Ambiguous", "Mismatch"] {
        assert!(
            workspace
                .navigate(&main, position(source, name), NavigationTarget::Definition)
                .is_empty(),
            "{name}"
        );
    }
}

#[test]
fn selected_target_context_invalidates_a_header_only_proof() {
    use pascal_lsp::workspace::projects::BuildField;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface uses Known; implementation end.";
    let known = "{$IF SizeOf(Extended)>=10}\nunit Known;\n{$ELSE}\nunit Other;\n{$ENDIF}\ninterface {$IF UnsupportedFunction(1)} var Value: Integer; {$ENDIF} implementation end.";
    write(&root.join("Main.pas"), source);
    write(&root.join("Known.pas"), known);
    write(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><Platform>Win32</Platform></PropertyGroup><ProjectExtensions><BorlandProject><Platforms><Platform value=\"Win32\">True</Platform><Platform value=\"Win64\">True</Platform></Platforms></BorlandProject></ProjectExtensions></Project>",
    );
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let project = Url::from_file_path(root.join("App.dproj")).unwrap();
    let mut workspace = workspace(root, "Win32");
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", known, "Known")]
    );
    workspace
        .select_build(&project, BuildField::Platform, Some("Win64"))
        .unwrap();
    assert!(
        workspace
            .navigate(
                &main,
                position(source, "Known"),
                NavigationTarget::Definition
            )
            .is_empty()
    );
    workspace
        .select_build(&project, BuildField::Platform, Some("Win32"))
        .unwrap();
    assert_eq!(
        workspace.navigate(
            &main,
            position(source, "Known"),
            NavigationTarget::Definition
        ),
        vec![header(root, "Known.pas", known, "Known")]
    );
}

#[test]
fn rename_audit_uses_the_same_initializer_include_decision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = "unit Main; interface const Revision={$I revision.inc}; {$IF Revision=7}{$I chosen.inc}{$ELSE}{$I forbidden.inc}{$ENDIF} const BadConst=1; implementation end.";
    write(&root.join("Main.pas"), source);
    write(&root.join("revision.inc"), "7");
    write(&root.join("chosen.inc"), "{ harmless chosen include }");
    let main = Url::from_file_path(root.join("Main.pas")).unwrap();
    let mut workspace = workspace(root, "Win32");
    let edit = workspace
        .rename_edits(&main, position(source, "BadConst"), "GoodConst", false)
        .unwrap();
    let changes = edit.changes.unwrap();
    let edits = changes.get(&main).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "GoodConst");
    assert_eq!(
        edits[0].range,
        header(root, "Main.pas", source, "BadConst").range
    );
}
