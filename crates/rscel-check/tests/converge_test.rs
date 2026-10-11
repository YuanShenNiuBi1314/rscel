//! C2 收敛测试（E010 分支不收敛 / E011 jump loop 跨 cell / E014 无出口无限循环）。
//!
//! 收敛定义（开发说明 §11.2）：cell body 的所有路径都必须到达 `exit` / `die`
//! 或跨 cell 转移；走到 body 末尾即报 E010。

use std::collections::BTreeSet;

use rscel_check::check_program;
use rscel_parser::Severity;

fn codes(src: &str) -> Vec<u16> {
    let out = rscel_parser::parse_program(src, 0);
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
fn two_conditional_branches_then_die_converge() {
    // 互斥双分支 + 末尾 die：两条 when 都可能落空 → 必须由 die 兜底收敛。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x) as (v);
jump b when v > 0 go with(v -> n);
jump b when v <= 0 go with((-v) -> n);
die;

cell b;
receive(n);
exit go with(n -> r);
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(!got.contains(&10), "合法互斥分支不应报 E010，实际 {got:?}");
}

/// 规格附录 A.5 的标准多分支互斥写法：N 条 `jump ... when` + 末尾 `die`。
#[test]
fn spec_a5_multi_branch_converges() {
    let src = r#"
entry start;

cell start;
receive(val: i64);
jump one when val == 1 go with(val -> n);
jump two when val == 2 go with(val -> n);
jump three when val == 3 go with(val -> n);
die;

cell one;
receive(n: i64);
exit go with();

cell two;
receive(n: i64);
exit go with();

cell three;
receive(n: i64);
exit go with();
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(!got.contains(&10), "A.5 多分支不应报 E010，实际 {got:?}");
}

/// if / else 形式：条件 jump 落空后走无条件的第二条（else 分支）。
#[test]
fn if_else_form_converges() {
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x) as (v);
jump b when v > 0 go with(v -> n);
jump b go with((-v) -> n);

cell b;
receive(n);
exit go with(n -> r);
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(!got.contains(&10), "if/else 形式不应报 E010，实际 {got:?}");
}

#[test]
fn fall_through_triggers_e010() {
    // cell 没有任何 exit/die/plain jump；走到末尾 → E010
    let src = r#"
cell main;
receive();
jump a;

cell a;
receive(x);
let z = 1;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(got.contains(&10), "期望 E010，实际 {got:?}");
}

#[test]
fn conditional_branches_without_fallback_trigger_e010() {
    // 与 `two_conditional_branches_then_die_converge` 对照：去掉末尾 die 后，
    // 两条 when 同时落空 → 走到 body 末尾 → E010。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x) as (v);
jump b when v > 0 go with(v -> n);
jump b when v <= 0 go with((-v) -> n);

cell b;
receive(n);
exit go with(n -> r);
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(got.contains(&10), "缺少兜底 die 应报 E010，实际 {got:?}");
}

#[test]
fn jump_loop_to_other_cell_triggers_e011() {
    let src = r#"
cell main;
receive();
jump a;

cell a;
receive(x);
jump loop b when x > 0 go with(x -> x);

cell b;
receive(x);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(got.contains(&11), "期望 E011，实际 {got:?}");
}

#[test]
fn infinite_loop_triggers_e014() {
    // 无 when 的 jump loop + 无 exit/die/plain → E014
    let src = r#"
cell main;
receive();
jump a;

cell a;
receive();
jump loop a;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(got.contains(&14), "期望 E014，实际 {got:?}");
}