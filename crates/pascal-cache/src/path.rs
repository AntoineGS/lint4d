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

#[cfg(test)]
mod tests {
    use crate::Encoder;
    use std::path::Path;

    fn encoded(path: &Path) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.encode(path);
        encoder.into_bytes()
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
