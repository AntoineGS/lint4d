# fmt4d

An opinionated Delphi / Object Pascal code formatter written in Rust.

Part of the [lint4d](https://github.com/AntoineGS/lint4d) workspace.

## Install

```bash
cargo install --path crates/fmt4d
```

Pre-built binaries: see [GitHub Releases](https://github.com/AntoineGS/lint4d/releases).

## Usage

```bash
# Format a file in place
fmt4d src/MyUnit.pas

# Check if a file is formatted (exit 1 if not)
fmt4d --check src/

# Show the diff instead of writing
fmt4d --diff src/MyUnit.pas

# Read stdin, write stdout
cat MyUnit.pas | fmt4d --stdin

# Create a default config file
fmt4d --init
```

## Configuration

fmt4d reads `.fmt4d.toml` from the nearest ancestor directory. Run `fmt4d --init` to create a default config with every option documented.

## Library API

```rust
use fmt4d::{format_source, FmtConfig};
use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

let source = b"unit Foo; interface end.";
let info = FileInfo::new(PathBuf::from("Foo.pas"));
let config = FmtConfig::default();
let formatted = format_source(source, &info, &config, &HashSet::new())?;
# Ok::<_, fmt4d::FmtError>(())
```

See the crate docs (`cargo doc --open -p fmt4d`) for the full API.

## License

MIT.
