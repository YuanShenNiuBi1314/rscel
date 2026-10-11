//! C7 over 闭包测试（E080：over 闭包必须是纯函数）。

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
fn closure_with_jump_triggers_e080() {
    // 闭包体含 DSL 关键字 `jump skip` → 闭包有副作用/控制流 → E080。
    let src = r#"
cell main;
receive();
jump a go with([1,2,3] -> coll);

cell a;
receive(coll);
jump b over coll |> filter(|x| jump skip when x > 1);

cell b;
receive(n);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        got.contains(&80),
        "期望错误码含 80(E080)，实际 {got:?}"
    );
}

#[test]
fn pure_closure_no_e080() {
    // 纯函数闭包 → 不报 E080。
    let src = r#"
cell main;
receive();
jump a go with([1,2,3] -> coll);

cell a;
receive(coll);
jump b over coll |> filter(|x| x > 1);

cell b;
receive(n);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&80),
        "纯闭包不应报 E080，实际 {got:?}"
    );
}