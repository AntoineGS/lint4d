use crate::{CanonicalEncode, Encoder};
use std::path::Path;

#[cfg(not(any(unix, windows)))]
compile_error!("pascal-cache encodes native paths only on Unix and Windows");

impl CanonicalEncode for Path {
    #[cfg(unix)]
    fn encode(&self, encoder: &mut Encoder) {
        use std::os::unix::ffi::OsStrExt;

        encoder.variant(1);
        encoder.bytes(self.as_os_str().as_bytes());
    }

    #[cfg(windows)]
    fn encode(&self, encoder: &mut Encoder) {
        use std::os::windows::ffi::OsStrExt;

        let units = self.as_os_str().encode_wide().collect::<Vec<u16>>();
        encoder.variant(2);
        encoder.seq(units, |encoder, unit| encoder.u16(unit));
    }
}

/// An upper bound of a path's canonical encoding, without encoding it: the
/// tag, the `u64` length, then the bytes on Unix, or two bytes per UTF-16
/// unit on Windows (a path never has more units than WTF-8 bytes).
pub(crate) fn path_bound(path: &Path) -> usize {
    let bytes = path.as_os_str().len();
    1 + 8 + if cfg!(windows) { 2 * bytes } else { bytes }
}

#[cfg(test)]
mod tests {
    use super::path_bound;
    use crate::Encoder;
    use std::path::Path;

    fn encoded(path: &Path) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.encode(path);
        encoder.into_bytes()
    }

    #[test]
    fn the_path_bound_is_the_encoded_length_on_unix_and_an_upper_bound_everywhere() {
        for path in ["", "/", "/w/Main.pas", "/w/caf\u{e9}.pas"] {
            let path = Path::new(path);
            assert!(path_bound(path) >= encoded(path).len(), "{path:?}");
            #[cfg(unix)]
            assert_eq!(path_bound(path), encoded(path).len(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths_encode_their_bytes_without_lossy_conversion() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            encoded(Path::new("/a")),
            [1, 2, 0, 0, 0, 0, 0, 0, 0, b'/', b'a']
        );
        let first = Path::new(OsStr::from_bytes(b"/x\xff"));
        let second = Path::new(OsStr::from_bytes(b"/x\xfe"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(encoded(first), encoded(second));
        assert_eq!(
            encoded(first),
            [1, 3, 0, 0, 0, 0, 0, 0, 0, b'/', b'x', 0xff]
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_spelling_is_preserved() {
        assert_eq!(Path::new("/a/./b"), Path::new("/a/b"));
        assert_ne!(encoded(Path::new("/a/./b")), encoded(Path::new("/a/b")));
        assert_ne!(
            encoded(Path::new("/w/Unit1.pas")),
            encoded(Path::new("/w/UNIT1.pas"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_encode_utf16_code_units() {
        assert_eq!(
            encoded(Path::new("C:")),
            [2, 2, 0, 0, 0, 0, 0, 0, 0, 0x43, 0, 0x3a, 0]
        );
    }
}
