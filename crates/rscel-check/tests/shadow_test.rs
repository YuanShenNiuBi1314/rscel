//! C5 遮蔽测试（E070）。

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

#[test]
fn let_x_shadows_name_e070() {
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
let x = 1;
die;
"#;
    let c = codes(src);
    assert!(c.contains(&70), "期望 E070，实际 {c:?}");
}

#[test]
fn let_different_name_no_shadow() {
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
let y = 1;
die;
"#;
    let c = codes(src);
    assert!(!c.contains(&70), "合法 let 不应报 E070，实际 {c:?}");
}