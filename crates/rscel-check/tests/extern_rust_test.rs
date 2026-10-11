//! C8 extern rust 边界测试（E090：extern rust 块内禁止 DSL 关键字）。

use std::collections::BTreeSet;

use rscel_check::check_program;
use rscel_parser::Severity;

fn codes(src: &str) -> Vec<u16> {
    let out = rscel_parser::parse_program(src, 0);
    // 必须先确认源码自身能解析：否则反向断言（"不应报 E0xx"）会在源码是语法垃圾时
    // 假通过——那是比测试失效更糟的情况。
    assert!(
        !out.has_errors(),
        "测试源码自身无法解析:\n{}",
        out.diagnostics
            .iter()
            .map(|d| d.render("test.rscel"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let r = check_program(&out.program);
    r.diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code)
        .collect()
}

#[test]
fn extern_rust_block_with_jump_triggers_e090() {
    // 块内 `jump other;` 以 DSL 关键字起始 → E090。
    let src = r#"
cell main;
receive();
extern rust {
    jump other;
};
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        got.contains(&90),
        "期望错误码含 90(E090)，实际 {got:?}"
    );
}

#[test]
fn extern_rust_read_block_with_die_triggers_e090() {
    // `extern rust read { die; }` 同样受 E090 约束（die 是 DSL 终结语句）。
    let src = r#"
cell main;
receive();
extern rust read {
    die;
};
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        got.contains(&90),
        "期望错误码含 90(E090)，实际 {got:?}"
    );
}

#[test]
fn pure_rust_block_no_e090() {
    // 纯 Rust 块 → 不报 E090。
    let src = r#"
cell main;
receive();
extern rust {
    let z = 1;
};
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&90),
        "纯 Rust 块不应报 E090，实际 {got:?}"
    );
}