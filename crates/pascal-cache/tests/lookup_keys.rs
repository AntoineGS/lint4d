use pascal_cache::{
    DiscoveryInputs, DiscoveryScope, KeyKind, LookupInputs, LookupKey, LookupKind,
    MANIFEST_DIGEST_CONTEXT, ScannedDirectory,
};
use std::path::Path;

fn lookup(
    kind: LookupKind,
    name: &str,
    expression: Option<&str>,
    importer: Option<&str>,
) -> LookupKey {
    LookupKey::new(&LookupInputs {
        kind,
        normalized_name: name,
        path_expression: expression,
        importer_directory: importer.map(Path::new),
    })
}

#[test]
fn lookup_keys_distinguish_every_field() {
    let base = lookup(LookupKind::Unit, "sysutils", None, None);
    assert_eq!(
        base,
        lookup(LookupKind::Unit, "sysutils", None, None),
        "deterministic"
    );
    assert_eq!(base.key().kind(), KeyKind::Lookup);
    for other in [
        lookup(LookupKind::Include, "sysutils", None, None),
        lookup(LookupKind::Unit, "classes", None, None),
        lookup(LookupKind::Unit, "SysUtils", None, None),
        lookup(LookupKind::Unit, "sysutils", Some("SysUtils.pas"), None),
        lookup(LookupKind::Unit, "sysutils", Some(""), None),
        lookup(LookupKind::Unit, "sysutils", None, Some("/w")),
    ] {
        assert_ne!(other.digest(), base.digest(), "{other:?}");
    }
}

#[test]
fn discovery_scopes_keep_directory_order_and_treat_exclusions_as_a_set() {
    let a = ScannedDirectory {
        path: Path::new("/a"),
        recursive: true,
    };
    let b = ScannedDirectory {
        path: Path::new("/b"),
        recursive: false,
    };
    let scope = |directories: &[ScannedDirectory<'_>], exclusions: &[&str]| {
        DiscoveryScope::new(&DiscoveryInputs {
            directories,
            exclusions,
        })
    };
    let base = scope(&[a, b], &["*.bak", "tmp"]);
    assert_eq!(base.key().kind(), KeyKind::Discovery);
    assert_eq!(
        base,
        scope(&[a, b], &["tmp", "*.bak", "tmp"]),
        "a set of exclusions"
    );
    assert_ne!(
        base,
        scope(&[b, a], &["*.bak", "tmp"]),
        "scan order matters"
    );
    assert_ne!(
        base,
        scope(
            &[
                ScannedDirectory {
                    recursive: false,
                    ..a
                },
                b
            ],
            &["*.bak", "tmp"]
        ),
        "recursion matters"
    );
    assert_ne!(base, scope(&[a, b], &["*.bak"]));
    assert_ne!(
        base,
        scope(&[a, b], &["*.BAK", "tmp"]),
        "patterns are exact"
    );
}

#[cfg(unix)]
#[test]
fn lookup_key_manifest_is_pinned() {
    let key = lookup(LookupKind::Include, "a.inc", Some("A.inc"), Some("/w"));
    let mut expected = vec![b'L', b'4', b'D', b'C', 1, 0, 0, 0, 7]; // header, Lookup
    expected.extend([1, 0, 2]); // kind Include
    expected.extend([2, 0, 5, 0, 0, 0, 0, 0, 0, 0]); // normalized name
    expected.extend(b"a.inc");
    expected.extend([3, 0, 1, 5, 0, 0, 0, 0, 0, 0, 0]); // path expression
    expected.extend(b"A.inc");
    expected.extend([4, 0, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0]); // importer directory, Unix path
    expected.extend(b"/w");
    assert_eq!(key.key().manifest(), expected);
    assert_eq!(
        *key.digest().as_bytes(),
        blake3::derive_key(MANIFEST_DIGEST_CONTEXT, &expected)
    );
}

#[cfg(unix)]
#[test]
fn discovery_scope_manifest_is_pinned() {
    let scope = DiscoveryScope::new(&DiscoveryInputs {
        directories: &[ScannedDirectory {
            path: Path::new("/w"),
            recursive: true,
        }],
        exclusions: &["b", "a", "b"],
    });
    let mut expected = vec![b'L', b'4', b'D', b'C', 1, 0, 0, 0, 8]; // header, Discovery
    expected.extend([1, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // one directory
    expected.extend([1, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0]); //   path
    expected.extend(b"/w");
    expected.extend([2, 0, 1]); //   recursive
    expected.extend([2, 0, 2, 0, 0, 0, 0, 0, 0, 0]); // two exclusions, sorted
    expected.extend([1, 0, 0, 0, 0, 0, 0, 0, b'a']);
    expected.extend([1, 0, 0, 0, 0, 0, 0, 0, b'b']);
    assert_eq!(scope.key().manifest(), expected);
    assert_eq!(
        *scope.digest().as_bytes(),
        blake3::derive_key(MANIFEST_DIGEST_CONTEXT, &expected)
    );
}
