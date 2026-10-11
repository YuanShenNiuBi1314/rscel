//! C4 所有权测试（E060 / E061 / E063 / E064）。
//!
//! 判据依据《RSupercell开发说明》§11.4 与附录 B：
//! - E060：变量已 move，重复使用；
//! - E061：冻结（read with 后）变量被 move；
//! - E063：小 POD 类型禁止 read with（判据是 `receive(x: i64)` 的**声明类型**）；
//! - E064：Arc 句柄出现在 go with 实参。

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

/// 断言错误码集合含 `expected`。
fn assert_has_code(src: &str, expected: u16, label: &str) {
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        got.contains(&expected),
        "[{label}] 期望错误码含 {expected}，实际 {got:?}"
    );
}

/// 断言错误码集合**不**含 `expected` 里的任何一个。
///
/// 这一侧的断言同等重要：C4 是**编译门禁**（`build` 遇 error 就停），
/// 误报等于拦住一个合法程序，而漏报仍有 rustc 的 E0382 兜底。
fn assert_no_codes(src: &str, expected: &[u16], label: &str) {
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    for code in expected {
        assert!(
            !got.contains(code),
            "[{label}] 期望不含 {code}，实际 {got:?}"
        );
    }
}

/// `cell a` 以 `receive(x)` 接收，`x` 在 receive 后立即被 `jump b go with(x -> y)` 移出。
const MOVE_THEN_USE: &str = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump b go with(x -> y);

cell b;
receive(y);
exit go with(y -> r);
exit go with(y -> x);
"#;

#[test]
fn move_then_use_triggers_e060() {
    // y 在 exit go with(y -> r) 被 move，随后的 exit go with(y -> x) 再次使用 → E060。
    //
    // 注：两条 `exit` 在运行时第二条不可达，但所有权检查按模块文档的保守过近似
    // 仍然继续推进状态机，故能报出 E060（见 ownership.rs 模块文档）。
    assert_has_code(MOVE_THEN_USE, 60, "E060");
}

#[test]
fn read_with_then_move_triggers_e061() {
    // `jump b read with(x -> y)` 让 x → Frozen；随后 `jump c go with(x -> x)` move 它 → E061。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump b read with(x -> y);
jump c go with(x -> x);

cell c;
receive(x);
die;

cell b;
receive(y);
die;
"#;
    assert_has_code(src, 61, "E061");
}

#[test]
fn small_pod_read_with_triggers_e063() {
    // §11.4 标准样例：x 声明为 i64（小 POD / Copy），却用 read with → E063。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x: i64);
jump b read with(x -> n);

cell b;
receive(n);
die;
"#;
    assert_has_code(src, 63, "E063");
}

#[test]
fn force_read_with_on_small_pod_is_not_e063() {
    // `force read with` 是用户对 E063 的显式豁免（改为 W060 告警），不应再报 E063。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x: i64);
jump b force read with(x -> n);

cell b;
receive(n);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&63),
        "force read with 不应报 E063，实际 {got:?}"
    );
}

#[test]
fn pod_read_with_allowed_when_type_unannotated() {
    // 无类型注解 → 不做 POD 判定（类型交给 rustc 的 Copy/move 规则），不应报 E063。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump b read with(x -> n);

cell b;
receive(n);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&63),
        "未标注类型不应报 E063，实际 {got:?}"
    );
}

#[test]
fn non_pod_read_with_is_allowed() {
    // String 是非 POD（非 Copy），`read with` 语义正确 → 不报 E063。
    let src = r#"
cell main;
receive();
jump a go with(String::new() -> x);

cell a;
receive(x: String);
jump b read with(x -> n);

cell b;
receive(n);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&63),
        "非 POD 的 read with 不应报 E063，实际 {got:?}"
    );
}

#[test]
fn arc_handle_in_go_with_triggers_e064() {
    // `Arc::new(0)` 含 "Arc::" → go with 实参是 Arc 句柄 → E064。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump b go with(Arc::new(0) -> h);

cell b;
receive(h);
die;
"#;
    assert_has_code(src, 64, "E064");
}

#[test]
fn pod_move_twice_is_allowed() {
    // §10.2：POD 是 Copy，可反复 `go with`，**不触发 E060**。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x: i64);
jump b go with(x -> y);
jump c go with(x -> z);

cell b;
receive(y);
die;

cell c;
receive(z);
die;
"#;
    let got: BTreeSet<u16> = codes(src).into_iter().collect();
    assert!(
        !got.contains(&60),
        "Copy 类型重复 go with 不应报 E060，实际 {got:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-2（docs/05）：`let` 绑定入状态表 + `over` 的集合是 move
// ---------------------------------------------------------------------------

/// `bench/pipeline_and.rscel` / `bench/read_share.rscel` 在 2026-10-10 之前的写法：
/// `over items` 之后又取 `items.len()`。§10.1 说 `over` 把集合 move 进组，所以这是
/// use-after-move —— 改前 `rscel check` **零诊断**，只有 rustc 的 E0382 会拒绝生成物。
const OVER_THEN_LEN: &str = r#"
cell main;
receive();
let items: Vec<i64> = (1..=4i64).collect();
jump and w over items concurrent_limit = 2 group g1;
jump sync g1;
let total = items.len();
exit go with();

cell w;
receive(x: i64);
exit go with();
"#;

#[test]
fn over_moves_the_collection_and_a_later_let_is_e062() {
    assert_has_code(OVER_THEN_LEN, 62, "E062");
}

#[test]
fn over_before_the_read_is_clean() {
    // 同一份程序把 `len()` 挪到 `over` **之前**（两个 bench 场景现在的写法）必须零诊断，
    // 否则 `over` 那条 move 就是在误报。
    let src = r#"
cell main;
receive();
let items: Vec<i64> = (1..=4i64).collect();
let total = items.len();
jump and w over items concurrent_limit = 2 group g1;
jump sync g1;
exit go with();

cell w;
receive(x: i64);
exit go with();
"#;
    assert_no_codes(src, &[60, 61, 62], "over 之前先读");
}

#[test]
fn over_twice_triggers_e060() {
    // 同一个集合被两次 `over` 消费：第二次是"使用已 Moved 的名字"（§10.3）→ E060。
    let src = r#"
cell main;
receive();
let items: Vec<i64> = (1..=4i64).collect();
jump and a over items group g1;
jump sync g1;
jump and b over items group g2;
jump sync g2;
exit go with();

cell a;
receive(x: i64);
exit go with();

cell b;
receive(x: i64);
exit go with();
"#;
    assert_has_code(src, 60, "E060");
}

#[test]
fn over_frozen_alias_triggers_e061() {
    // `read with(items)` 让别名冻结，此后 `over items` 是"冻结变量被 move"→ E061。
    let src = r#"
cell main;
receive(items: Vec<i64>);
jump a read with(items);
jump and b over items group g1;
jump sync g1;
exit go with();

cell a;
receive(items: Arc<Vec<i64>>);
exit go with();

cell b;
receive(x: i64);
exit go with();
"#;
    assert_has_code(src, 61, "E061");
}

#[test]
fn let_binding_sent_twice_is_e060() {
    // 非 Copy 的 `let` 变量被两次 `go with`：与未注解的 receive 别名同口径 → E060。
    let src = r#"
cell main;
receive();
let data: Vec<i64> = vec![1i64, 2i64];
jump a go with(data -> x);
jump b go with(data -> y);

cell a;
receive(x);
die;

cell b;
receive(y);
die;
"#;
    assert_has_code(src, 60, "E060");
}

#[test]
fn pod_annotated_let_is_not_move_tracked() {
    // §10.2 的豁免要能落在 `let` 上：写了 `let n: i64` 就是 Copy，反复传值不报。
    // 这条同时钉住"Copy 是状态而不是回头查 receive 表"这个实现选择——只查 receive
    // 的话，`let n: i64` 会被当成非 Copy，第二次 `go with` 就成了误报。
    let src = r#"
cell main;
receive();
let n: i64 = 5;
jump a go with(n -> x);
jump b go with(n -> y);
println!("n={}", n);
exit go with();

cell a;
receive(x);
die;

cell b;
receive(y);
die;
"#;
    assert_no_codes(src, &[60, 62], "POD 注解的 let");
}

#[test]
fn rebinding_a_moved_let_restores_usability() {
    // `let` 重新绑定 = Rust 的遮蔽，此后引用的是新值，不该再报。
    let src = r#"
cell main;
receive();
let data: Vec<i64> = vec![1i64];
jump a go with(data -> x);
let data: Vec<i64> = vec![2i64];
println!("len={}", data.len());
exit go with();

cell a;
receive(x);
die;
"#;
    assert_no_codes(src, &[60, 62], "重新绑定");
}

#[test]
fn lookalike_identifiers_are_not_uses() {
    // 引用判定必须是**词**级别的。旧实现用 `text.contains(alias)`，于是别名 `x` 会被
    // `max`、`holder.x`、字符串 `"x"`、注释里的 `x` 命中 —— 那是拦住合法程序的误报。
    let src = r#"
cell main;
receive();
let x: Vec<i64> = vec![1i64];
jump a go with(x -> v);
let max = 3i64;
let x2 = max;
let holder = RawData { x: 0 };
let s = "x";
println!("{} {} {}", max + x2, holder.x, s);
exit go with();

cell a;
receive(v);
die;
"#;
    assert_no_codes(src, &[60, 62], "同形不同词");
}

#[test]
fn compound_go_with_arg_reading_a_moved_value_is_e062() {
    // `go with(items -> v)` 之后 `go with(items.len() -> n)`：复合表达式不移动任何东西，
    // 但它是一次**读取**，读的是已经移出去的名字。
    let src = r#"
cell main;
receive();
let items: Vec<i64> = (1..=4i64).collect();
jump a go with(items -> v);
jump b go with(items.len() -> n);

cell a;
receive(v);
die;

cell b;
receive(n);
die;
"#;
    assert_has_code(src, 62, "E062");
}

#[test]
fn out_after_over_is_e062() {
    // `out(...)` 的实参也是透传文本。
    let src = r#"
cell main;
receive();
let items: Vec<i64> = (1..=4i64).collect();
jump and w over items group g1;
jump sync g1;
out("left=", items.len());
exit go with();

cell w;
receive(x: i64);
exit go with();
"#;
    assert_has_code(src, 62, "E062");
}

#[test]
fn jump_loop_when_condition_is_not_a_use_after_move() {
    // §10.4 的标准样例：`let` 变量每轮重新绑定，`jump loop .. when next > 0
    // go with(next, next_acc)` 的 `when` 在本轮 move **之前**求值，而 fallthrough 走的是
    // "条件为假"那条路——`go with(...)` 根本没执行，所以循环后的 `next_acc` 仍是可用值。
    // 这条钉住两件事：引用判定用移动前的状态、`jump loop` 的 fallthrough 回滚到语句之前。
    let src = r#"
cell main;
receive();
jump fact go with(3i64 -> n, 1i64 -> acc);

cell fact;
receive(n: i64, acc: i64) as (cur, sum);
let next = cur - 1;
let next_acc = sum * cur;
jump loop fact when next > 0 go with(next, next_acc);
println!("r={}", next_acc);
exit go with(next_acc -> result);
"#;
    assert_no_codes(src, &[60, 62], "jump loop 的 when 与 fallthrough");
}

#[test]
fn read_with_after_move_triggers_e060() {
    // 已移出的名字再走 `read with`（要生成 `Arc::clone(&x)`）同样是 use-after-move。
    let src = r#"
cell main;
receive();
let data: Vec<i64> = vec![1i64, 2i64];
jump a go with(data -> x);
jump b read with(data);

cell a;
receive(x);
die;

cell b;
receive(data: Arc<Vec<i64>>);
die;
"#;
    assert_has_code(src, 60, "E060");
}
