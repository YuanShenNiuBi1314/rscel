//! C6 可达性测试（E020 / W001）。

use rscel_check::check_program;
use rscel_parser::Severity;

fn codes(src: &str) -> Vec<u16> {
    let out = rscel_parser::parse_program(src, 0);
    let r = check_program(&out.program);
    r.diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code)
        .collect()
}

fn warnings(src: &str) -> Vec<u16> {
    let out = rscel_parser::parse_program(src, 0);
    let r = check_program(&out.program);
    r.diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.code)
        .collect()
}

#[test]
fn no_entry_triggers_e020() {
    let src = r#"
cell a;
receive();
die;
"#;
    let c = codes(src);
    assert!(c.contains(&20), "期望 E020，实际 {c:?}");
}

#[test]
fn entry_decl_triggers_no_e020() {
    let src = r#"
entry a;
cell a;
receive();
die;
"#;
    let c = codes(src);
    assert!(!c.contains(&20), "合法 entry 不应报 E020，实际 {c:?}");
}

#[test]
fn unreachable_cell_triggers_w001() {
    // b 无任何入边，且不在 main 闭包中 → W001
    let src = r#"
cell main;
receive();
die;

cell orphan;
receive();
die;
"#;
    let w = warnings(src);
    assert!(w.contains(&1), "期望 W001，实际 {w:?}");
}

#[test]
fn reachable_cell_no_w001() {
    let src = r#"
cell main;
receive();
jump a;

cell a;
receive();
die;
"#;
    let w = warnings(src);
    assert!(!w.contains(&1), "可达 cell 不应报 W001，实际 {w:?}");
}