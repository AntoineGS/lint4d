use lint4d::discovery::discover_files;
use lint4d::engine::FileType;
use std::fs;
use tempfile::TempDir;

#[test]
fn discovers_pas_files_in_directory() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Unit1.pas"), "unit Unit1;").unwrap();
    fs::write(dir.path().join("Unit2.pas"), "unit Unit2;").unwrap();
    fs::write(dir.path().join("readme.txt"), "not a pas file").unwrap();

    let files = discover_files(&[dir.path().to_path_buf()], &[]).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.file_type == FileType::Pas));
}

#[test]
fn discovers_dpr_and_dpk_files() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Project.dpr"), "program Project;").unwrap();
    fs::write(dir.path().join("Package.dpk"), "package Package;").unwrap();

    let files = discover_files(&[dir.path().to_path_buf()], &[]).unwrap();
    assert_eq!(files.len(), 2);
    let types: Vec<FileType> = files.iter().map(|f| f.file_type).collect();
    assert!(types.contains(&FileType::Dpr));
    assert!(types.contains(&FileType::Dpk));
}

#[test]
fn discovers_files_recursively() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(dir.path().join("Top.pas"), "unit Top;").unwrap();
    fs::write(sub.join("Sub.pas"), "unit Sub;").unwrap();

    let files = discover_files(&[dir.path().to_path_buf()], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn excludes_matching_patterns() {
    let dir = TempDir::new().unwrap();
    let gen = dir.path().join("generated");
    fs::create_dir(&gen).unwrap();
    fs::write(dir.path().join("Unit1.pas"), "unit Unit1;").unwrap();
    fs::write(gen.join("Generated.pas"), "unit Generated;").unwrap();

    let files = discover_files(
        &[dir.path().to_path_buf()],
        &["generated/**".to_string()],
    ).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].path.to_str().unwrap().contains("Unit1"));
}

#[test]
fn single_file_argument() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("Single.pas");
    fs::write(&file_path, "unit Single;").unwrap();

    let files = discover_files(&[file_path], &[]).unwrap();
    assert_eq!(files.len(), 1);
}

#[test]
fn results_sorted_by_path() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Zebra.pas"), "unit Zebra;").unwrap();
    fs::write(dir.path().join("Alpha.pas"), "unit Alpha;").unwrap();
    fs::write(dir.path().join("Mid.pas"), "unit Mid;").unwrap();

    let files = discover_files(&[dir.path().to_path_buf()], &[]).unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.path.file_name().unwrap().to_str().unwrap()).collect();
    assert_eq!(names, vec!["Alpha.pas", "Mid.pas", "Zebra.pas"]);
}
