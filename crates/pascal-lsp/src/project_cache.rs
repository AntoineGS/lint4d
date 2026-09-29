//! Project-scoped cache shared by analysis workers and the warmer.

/// Default for the `maxCacheBytes` limit.
#[allow(dead_code)]
pub(crate) const DEFAULT_MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Retained heap bytes per indexed source byte, measured by
/// `measure_retained_bytes_per_source_byte` on RAD Studio 7.0 RTL/VCL units.
#[allow(dead_code)]
pub(crate) const RETAINED_BYTES_PER_SOURCE_BYTE: usize = 48; // measured 2026-09-29: 46, 45, 45 on RAD Studio 7.0 RTL/VCL

#[cfg(test)]
mod tests {
    fn resident_bytes() -> usize {
        let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
        let pages: usize = statm
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .expect("resident pages");
        pages * 4096
    }

    /// Run manually:
    /// `PASCAL_RTL=~/.local/share/Headless-Delphi/files/RAD\ Studio/7.0/source/Win32 \
    ///  cargo test -p pascal-lsp --release --lib -- --ignored measure_retained --nocapture`
    #[test]
    #[ignore = "manual measurement against local Delphi sources"]
    fn measure_retained_bytes_per_source_byte() {
        let root = std::path::PathBuf::from(std::env::var("PASCAL_RTL").expect("PASCAL_RTL"));
        let files = [
            "rtl/win/Windows.pas",
            "rtl/sys/SysUtils.pas",
            "rtl/common/Classes.pas",
            "vcl/Controls.pas",
            "vcl/Forms.pas",
            "db/DB.pas",
        ];
        let mut index = crate::navigation::NavigationIndex::new();
        let context = pascal_core::conditional::ConditionalContext::default();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let before = resident_bytes();
        let mut source_bytes = 0usize;
        for file in files {
            let path = root.join(file);
            let bytes = std::fs::read(&path).expect("source");
            let text = crate::workspace::resolver::decode_source_bytes(&bytes);
            source_bytes += text.len();
            let uri = lsp_types::Url::from_file_path(&path).expect("uri");
            index
                .update_with_context_and_cached_with_cancel(uri, text, &context, None, &cancel)
                .expect("index");
        }
        let after = resident_bytes();
        let factor = (after.saturating_sub(before)) / source_bytes.max(1);
        println!(
            "source={source_bytes} retained={} factor={factor}",
            after - before
        );
        std::hint::black_box(&index);
    }
}
