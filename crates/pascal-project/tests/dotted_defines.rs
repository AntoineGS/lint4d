//! l4d-onnm fix round 1 (I2): the DCC task passes DCC_Define as `-D<list>`
//! (Borland.Build.Tasks.Delphi.dll option table), and dcc32 21.0 defines
//! every identifier of a `-D` item: `-DX.Y;Z` defines X, Y and Z, `-DX-Y`
//! defines X and Y (`~/.cache/l4d-tmp/d46-dccprobe/U.dpr`).
use pascal_project::{ConditionalFact, ProjectContext};
use std::fs;

#[test]
fn a_dotted_project_define_defines_each_identifier() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("App.dpr");
    fs::write(&main, "program App; begin end.").unwrap();
    fs::write(
        root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource>\
         <DCC_Define>W.V;X-Y;PLAIN</DCC_Define></PropertyGroup></Project>",
    )
    .unwrap();
    let context = ProjectContext::discover(&main, &[root.to_path_buf()], &Default::default())
        .unwrap()
        .context;
    let conditional = context.conditional_context();
    for name in ["W", "V", "X", "Y", "PLAIN"] {
        assert_eq!(conditional.define(name), ConditionalFact::True, "{name}");
    }
    assert!(
        !conditional.defines.contains_key("W.V"),
        "{:?}",
        conditional.defines
    );
}
