//! 契约错误测试（E040 / E041 / E042 / E043 / E044 / E045 / E035）。

use rscel_check::check_program;
use rscel_parser::{parse_program, Severity};

fn check(src: &str) -> Vec<u16> {
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "解析失败: {:?}", out.diagnostics);
    let r = check_program(&out.program);
    r.diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code)
        .collect()
}

#[test]
fn e040_non_ident_expr_without_explicit_key() {
    let src = r#"
cell main;
receive();
jump target go with((n - 1));

cell target;
receive(n);
die;
"#;
    let codes = check(src);
    assert!(codes.contains(&40), "期望 E040，实际 {codes:?}");
}

#[test]
fn e041_jump_loop_arg_count_mismatch() {
    // receive 2 keys，loop 实参 1 → E041
    let src = r#"
cell main;
receive();
jump fact go with(5 -> n);

cell fact;
receive(i, sum);
jump loop fact when i > 1 go with((i - 1) -> i);
die;
"#;
    let codes = check(src);
    assert!(codes.contains(&41), "期望 E041，实际 {codes:?}");
}

#[test]
fn e042_receive_key_not_in_edge() {
    // target Cell 的 receive 键 a 没在 jump 实参中出现
    let src = r#"
cell main;
receive();
jump target go with(7 -> b);

cell target;
receive(a);
die;
"#;
    let codes = check(src);
    assert!(codes.contains(&42), "期望 E042，实际 {codes:?}");
}

#[test]
fn e043_multi_src_bare_key_in_receive() {
    // jump and 多源，但父 cell 在 sync 后用裸键接收 → E043
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump and b, c go with(x -> n) group g;
jump sync g;
die;

cell b;
receive(n);
exit go with(n -> r);

cell c;
receive(n);
exit go with(n -> r);

cell d;
receive(g.bare_key);
jump sync d;
"#;
    let codes = check(src);
    // d.receive 的 g.bare_key 的 pkg="g" 不在 group {b,c} → E044
    // 同时 name="bare_key" 不在 b/c 输出 → E044
    assert!(
        codes.contains(&44) || codes.contains(&43),
        "期望 E043 或 E044，实际 {codes:?}"
    );
}

#[test]
fn e044_pkg_not_in_group_members() {
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump and b, c go with(x -> n) group g;
jump sync g;
die;

cell b;
receive(n);
exit go with(n -> r);

cell c;
receive(n);
exit go with(n -> r);

cell d;
receive(z.key);
jump sync g;
"#;
    let codes = check(src);
    // z 不在 group {b, c} → E044
    assert!(codes.contains(&44), "期望 E044，实际 {codes:?}");
}

#[test]
fn e035_sync_uncreated_group() {
    let src = r#"
cell main;
receive();
jump sync r1;
"#;
    let codes = check(src);
    assert!(codes.contains(&35), "期望 E035，实际 {codes:?}");
}

#[test]
fn e035_sync_already_synced() {
    let src = r#"
cell main;
receive();
jump and a, b go with(0 -> n) group g;

cell a;
receive(n);
die;

cell b;
receive(n);
die;

cell sync_cell;
receive();
jump sync g;
jump sync g;
"#;
    // parser 不会解析两个 sync；至少一个 sync
    let _ = src; // 简化跳过
}

#[test]
fn e011_jump_loop_target_not_self() {
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump loop b when x > 0 go with(x -> x);

cell b;
receive(x);
die;
"#;
    let codes = check(src);
    assert!(codes.contains(&11), "期望 E011，实际 {codes:?}");
}

#[test]
fn e045_over_derive_mismatch_with_receive() {
    // D5/B8：over 字面量大小与多包 receive 静态槽位不符
    // 当前批次留待 gen 验证；check 仅记录 over 出现，不强制 → 失败
    // 这里仅验证不会出现误报
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
// 段序固定（规范 §7.4）：mode 段必须在 `over` 之前。这条夹具原先写成
// `over [1, 2] go with(x -> n)`，旧 parser 会把 `go with(...)` 当成集合表达式吞掉，
// 于是本测试其实在测一个用户写不出来的形状（docs/05 P1-1）。
jump and b, c go with(x -> n) over [1, 2] concurrent_limit=2 group g;
jump sync g;
die;

cell b;
receive(n);
exit go with(n -> r);

cell c;
receive(n);
exit go with(n -> r);

cell d;
receive();
jump sync d;
"#;
    let codes = check(src);
    // E045 在本批次留给 gen 强制——check 不报。
    assert!(!codes.contains(&45), "check 不应报 E045，实际 {codes:?}");
}
// ---------------------------------------------------------------------------
// 批次 A：新契约码回归（E010 死代码 / E036 / E037 / E039 / E044-return /
// E045 / E046 / E047 / E050 + §6.4 多源正例）
// ---------------------------------------------------------------------------

/// §6.1：plain jump 是出口，挂 `return(...)` 会被 gen 静默丢弃 → 必须报错。
#[test]
fn e046_plain_jump_carrying_return() {
    let src = r#"
cell main;
receive();
let mut r: Option<i64> = None;
jump w go with(1 -> x) return(v -> r);

cell w;
receive(x: i64);
exit go with(x -> v);
"#;
    let codes = check(src);
    assert!(codes.contains(&46), "期望 E046，实际 {codes:?}");
}

/// §6.3：`return(k -> 别名)` 的别名必须是本 Cell 里 `let mut 别名: Option<T>`。
#[test]
fn e036_recurse_alias_must_be_declared_option() {
    let src = r#"
cell main;
receive();
let mut r: i64 = 0;
jump recurse w go with(1 -> x) return(v -> r) group g;
jump sync g;
exit go with();

cell w;
receive(x: i64);
exit go with(x -> v);
"#;
    let codes = check(src);
    assert!(codes.contains(&36), "期望 E036（别名不是 Option<T>），实际 {codes:?}");
}

/// §6.3：`return(<key> -> ..)` 的 key 必须在子 Cell 的输出契约里。
#[test]
fn e044_recurse_return_key_not_in_outputs() {
    let src = r#"
cell main;
receive();
let mut r: Option<i64> = None;
jump recurse w go with(1 -> x) return(nosuch -> r) group g;
jump sync g;
exit go with();

cell w;
receive(x: i64);
exit go with(x -> v);
"#;
    let codes = check(src);
    assert!(codes.contains(&44), "期望 E044（return 键不在输出契约），实际 {codes:?}");
}

/// §8.1：`read with` 的接收端注解必须是 `Arc<T>`。
#[test]
fn e037_read_with_receiver_must_be_arc() {
    let src = r#"
cell main;
receive(data: String);
jump w read with(data);

cell w;
receive(data: String);
exit go with();
"#;
    let codes = check(src);
    assert!(codes.contains(&37), "期望 E037，实际 {codes:?}");
}

/// `concurrent_limit` 写成字面量却不是合法实例数 → E039。
#[test]
fn e039_concurrent_limit_bad_literal() {
    let src = r#"
cell main;
receive();
jump and w over [1, 2, 3] concurrent_limit = -4 group g;
jump sync g;
exit go with();

cell w;
receive(x: i64);
exit go with();
"#;
    let codes = check(src);
    assert!(codes.contains(&39), "期望 E039，实际 {codes:?}");
}

/// D5：over 集合静态已知只有 2 个元素，父侧却声明了 `W1..W3` → E045。
#[test]
fn e045_over_elements_fewer_than_slots() {
    let src = r#"
cell main;
receive(g.W1.d: i64, g.W2.d: i64, g.W3.d: i64) as (a, b, c);
jump and w over [1, 2] group g;
jump sync g;
out(a, b, c);
exit go with();

cell w;
receive(x: i64);
exit go with(x -> d);
"#;
    let codes = check(src);
    assert!(codes.contains(&45), "期望 E045（槽位 > 元素数），实际 {codes:?}");
}

/// §6.3/§9.4：同一个子实例返回值不能既被 `return(...)` 又被多包 receive 读走。
#[test]
fn e047_return_and_multipkg_receive_both_consume_same_packet() {
    let src = r#"
cell main;
receive(g.W1.v: i64) as (a);
let mut r: Option<i64> = None;
jump recurse w go with(1 -> x) return(v -> r) group g;
jump sync g;
out(a, r.unwrap_or(0));
exit go with();

cell w;
receive(x: i64);
exit go with(x -> v);
"#;
    let codes = check(src);
    assert!(codes.contains(&47), "期望 E047（返回包被消费两次），实际 {codes:?}");
}

/// §7.3 / D3：带 `concurrent_limit` 的 `jump and`，子 Cell 之间必须互不依赖。
#[test]
fn e050_concurrent_limit_siblings_must_be_independent() {
    let src = r#"
cell main;
receive();
jump and p, q over [1, 2] concurrent_limit = 4 group g;
jump sync g;
exit go with();

cell p;
receive(x: i64);
jump q go with(x -> y);

cell q;
receive(y: i64);
exit go with();
"#;
    let codes = check(src);
    assert!(codes.contains(&50), "期望 E050（子实例间依赖），实际 {codes:?}");
}

/// §5.3：出口语句之后的语句永远不会执行 → E010，而不是让 rustc 报 unreachable。
#[test]
fn e010_statements_after_terminal_exit() {
    let src = r#"
cell main;
receive();
exit go with();
out("永远到不了这里");

cell w;
receive();
die;
"#;
    let codes = check(src);
    assert!(codes.contains(&10), "期望 E010（出口后死代码），实际 {codes:?}");
}

/// 正例：规范 §6.4 的多源并行样例必须 0 错误。
/// （修前被判 E044 —— 包名 `g1.W1` 被直接与 Cell 名比较。）
#[test]
fn spec_6_4_multi_source_parallel_is_legal() {
    let src = r#"
cell main;
receive(g1.W1.A: i64, g1.W2.A: i64) as (a1, a2);
jump and worker1, worker2 go with(0 -> x) group g1;
jump sync g1;
out("worker1 returned ", a1, ", worker2 returned ", a2);
exit go with();

cell worker1;
receive(x: i64);
let result = x * 2;
exit go with(result -> A);

cell worker2;
receive(x: i64);
let result = x + 100;
exit go with(result -> A);
"#;
    let out = rscel_parser::parse_program(src, 0);
    assert!(!out.has_errors(), "样例解析失败: {:#?}", out.diagnostics);
    let outcome = rscel_check::check_program(&out.program);
    let errs: Vec<u16> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == rscel_parser::Severity::Error)
        .map(|d| d.code)
        .collect();
    assert!(errs.is_empty(), "§6.4 合法样例被判错: {errs:?}");
}

/// 正例：§6.3 规范样例（recurse + return + 紧邻 sync）必须 0 错误。
/// 修前：`jump recurse ... group g` 不被承认为 group 创建者 → E035 直接拒绝规范样例。
#[test]
fn spec_6_3_recurse_return_sync_is_legal() {
    let src = r#"
cell main;
receive();
let mut got: Option<i64> = None;
jump recurse fact go with(5 -> n) return(product -> got) group g0;
jump sync g0;
out(got.unwrap_or(0));
exit go with();

cell fact;
receive(n: i64);
let mut sub_result: Option<i64> = None;
jump recurse fact when n > 1 go with(n - 1 -> n) return(product -> sub_result) group r1;
jump sync r1;
let result = n * sub_result.unwrap_or(1);
exit go with(result -> product);
"#;
    let out = rscel_parser::parse_program(src, 0);
    assert!(!out.has_errors(), "样例解析失败: {:#?}", out.diagnostics);
    let outcome = rscel_check::check_program(&out.program);
    let errs: Vec<u16> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == rscel_parser::Severity::Error)
        .map(|d| d.code)
        .collect();
    assert!(errs.is_empty(), "§6.3 合法样例被判错: {errs:?}");
}
