//! parser 的 AST 断言矩阵（§18.1：每个 DSL 语句的解析样例）。
//!
//! 断言精确到 AST 字段：`RawRust.text` 逐字相等、`JumpStmt.kind`、包限定键解析结果、
//! `Arg.key`、诊断错误码等。

use rscel_parser::ast::*;
use rscel_parser::diag::code;
use rscel_parser::parse_program;

fn ok(src: &str) -> Program {
    let out = parse_program(src, 7);
    assert!(!out.has_errors(), "期望无错误，实际: {:?}", out.diagnostics.iter().map(|d| d.render("t.rscel")).collect::<Vec<_>>());
    out.program
}

fn codes(src: &str) -> Vec<u16> {
    parse_program(src, 7).diagnostics.iter().map(|d| d.code).collect()
}

fn first_cell(src: &str) -> CellDef {
    ok(src).cells().next().expect("至少一个 cell").clone()
}

fn body(src: &str) -> Vec<Stmt> {
    first_cell(src).body
}

fn only_stmt(src: &str) -> Stmt {
    let b = body(src);
    assert_eq!(b.len(), 1, "期望恰好一条语句，实际 {b:?}");
    b.into_iter().next().expect("非空")
}

fn as_jump(s: &Stmt) -> &JumpStmt {
    match s {
        Stmt::Jump(j) => j,
        other => panic!("期望 Jump，实际 {other:?}"),
    }
}

fn raw_text(s: &Stmt) -> &str {
    match s {
        Stmt::RawRust(r) => r.text.as_str(),
        other => panic!("期望 RawRust，实际 {other:?}"),
    }
}

// ------------------------------------------------------------------ program

#[test]
fn program_items_entry_and_lookup() {
    let p = ok("entry start;\ncell start;\nreceive();\ndie;\ncell other;\nreceive();\ndie;\n");
    assert_eq!(p.entry.as_deref(), Some("start"));
    assert_eq!(p.items.len(), 2);
    assert_eq!(p.cell("start").unwrap().name, "start");
    assert_eq!(p.cell("other").unwrap().body.len(), 1);
    assert!(p.cell("nope").is_none());
    assert_eq!(p.cells().count(), 2);
}

#[test]
fn comments_and_blank_lines_have_no_meaning() {
    // §3.1：换行仅为阅读美化；一行可写多条语句
    let p = ok("cell a; receive(x); /* c1 */ // c2\n die; let y = 1; ");
    assert_eq!(p.cell("a").unwrap().body.len(), 2);
}

// ------------------------------------------------------------------ receive

#[test]
fn receive_empty_bare_as_typed_and_mixed() {
    // §3.4.1 全部五种形态
    let empty = first_cell("cell a;\nreceive();\ndie;\n");
    assert!(empty.receive.keys.is_empty());
    assert!(empty.receive.aliases.is_empty());

    let bare = first_cell("cell a;\nreceive(a, b);\ndie;\n");
    assert_eq!(bare.receive.keys.len(), 2);
    assert!(bare.receive.keys[0].pkg.is_none());
    assert_eq!(bare.receive.keys[0].name, "a");
    // 省略 as 时别名 = 键名（§3.4.1 规则 2）
    assert_eq!(bare.receive.aliases, vec!["a".to_string(), "b".to_string()]);

    let aliased = first_cell("cell a;\nreceive(a, b) as (x, y);\ndie;\n");
    assert_eq!(aliased.receive.aliases, vec!["x".to_string(), "y".to_string()]);
    assert!(aliased.receive.keys[0].ty.is_none());

    let typed = first_cell("cell a;\nreceive(n: i64) as (num);\ndie;\n");
    assert_eq!(typed.receive.keys[0].ty.as_ref().unwrap().text, "i64");
    assert_eq!(typed.receive.aliases, vec!["num".to_string()]);

    // 别名上的类型注解（§3.4.1 示例 receive(n: i64) as (num: i64);）
    let both = first_cell("cell a;\nreceive(n: i64) as (num: i64);\ndie;\n");
    assert_eq!(both.receive.keys[0].ty.as_ref().unwrap().text, "i64");
    assert_eq!(both.receive.aliases, vec!["num".to_string()]);

    let mixed = first_cell("cell a;\nreceive(a, b: Vec<u8>) as (x, y);\ndie;\n");
    assert!(mixed.receive.keys[0].ty.is_none());
    assert_eq!(mixed.receive.keys[1].ty.as_ref().unwrap().text, "Vec<u8>");
}

#[test]
fn receive_packet_key_split_rule() {
    // 裁定：pkg = 最后一个点之前的全部文本，name = 最后一段
    let c = first_cell("cell a;\nreceive(g1.W1.A, g1.W2.B, bare);\ndie;\n");
    assert_eq!(c.receive.keys[0].pkg.as_deref(), Some("g1.W1"));
    assert_eq!(c.receive.keys[0].name, "A");
    assert_eq!(c.receive.keys[1].pkg.as_deref(), Some("g1.W2"));
    assert_eq!(c.receive.keys[1].name, "B");
    assert!(c.receive.keys[2].pkg.is_none());
    assert_eq!(c.receive.keys[2].name, "bare");
    // 省略 as 时别名去掉包前缀（§3.4.1 规则 2）
    assert_eq!(c.receive.aliases, vec!["A".to_string(), "B".to_string(), "bare".to_string()]);
}

#[test]
fn receive_inline_arrow_alias_from_spec_1_2() {
    // §1.2 示例 receive(order -> ord);：行内别名归一到 aliases
    let c = first_cell("cell a;\nreceive(order -> ord);\ndie;\n");
    assert_eq!(c.receive.keys[0].name, "order");
    assert_eq!(c.receive.aliases, vec!["ord".to_string()]);
}

#[test]
fn receive_without_parentheses_is_accepted() {
    // 文档缺陷兼容：《语言设计文档.md》§5.2/§5.3/§7.6、附录 A.2/A.4/A.6 全部写作
    // `receive a, b;`（无括号），而 §3.3 文法要求 `receive(a, b);`。两种写法等价。
    let bare = first_cell("cell count_loop;\nreceive i, sum;\njump loop count_loop;\n");
    assert_eq!(bare.receive.keys.len(), 2);
    assert_eq!(bare.receive.keys[0].name, "i");
    assert_eq!(bare.receive.aliases, vec!["i".to_string(), "sum".to_string()]);

    let bare_as = first_cell("cell T;\nreceive x, log as (val, trace_id);\ndie;\n");
    assert_eq!(bare_as.receive.keys.len(), 2);
    assert_eq!(bare_as.receive.aliases, vec!["val".to_string(), "trace_id".to_string()]);

    let bare_single = first_cell("cell worker;\nreceive data;\nexit go with(A);\n");
    assert_eq!(bare_single.receive.keys[0].name, "data");

    let bare_empty = first_cell("cell main;\nreceive;\ndie;\n");
    assert!(bare_empty.receive.keys.is_empty());

    // 带类型注解的无括号形式
    let bare_typed = first_cell("cell c;\nreceive n: i64 as (num);\ndie;\n");
    assert_eq!(bare_typed.receive.keys[0].ty.as_ref().unwrap().text, "i64");
    assert_eq!(bare_typed.receive.aliases, vec!["num".to_string()]);
}

// ------------------------------------------------------------------ jump

#[test]
fn jump_plain_with_when_and_go_with() {
    // §3.4.2 ①
    let s = only_stmt("cell a;\nreceive(val);\njump cell_odd when val % 2 == 1 go with(val);\n");
    let j = as_jump(&s);
    assert_eq!(j.kind, JumpKind::Plain);
    assert_eq!(j.targets, vec!["cell_odd".to_string()]);
    assert_eq!(j.condition.as_ref().unwrap().text, "val % 2 == 1");
    assert_eq!(j.mode, Some(TransMode::GoWith));
    assert_eq!(j.args.len(), 1);
    assert_eq!(j.args[0].expr.text, "val");
    assert_eq!(j.args[0].key, None);
    assert_eq!(j.args[0].effective_key(), "val");
    assert!(j.ret_map.is_empty());
    assert!(j.over.is_none());
    assert!(j.concurrent_limit.is_none());
    assert!(j.group.is_none());
}

#[test]
fn jump_loop_is_own_kind() {
    // §3.4.2 ②：实参按位置刷新，不需要显式键名
    let s = only_stmt("cell a;\nreceive(i, sum);\njump loop count_loop when i < 5 go with(i + 1, sum + i);\n");
    let j = as_jump(&s);
    assert_eq!(j.kind, JumpKind::Loop);
    assert_eq!(j.targets, vec!["count_loop".to_string()]);
    assert_eq!(j.condition.as_ref().unwrap().text, "i < 5");
    assert_eq!(j.args.len(), 2);
    assert_eq!(j.args[0].expr.text, "i + 1");
    assert_eq!(j.args[1].expr.text, "sum + i");
    // §9.1：非标识符实参缺显式键名（由 rscel-check 报 E040），parser 只给依据
    assert!(j.args[0].needs_explicit_key());
}

#[test]
fn jump_recurse_with_return_map_and_group() {
    // §3.4.2 ③ / 附录 A.1
    let s = only_stmt("cell fact;\nreceive(n) as (num);\njump recurse fact when num > 1 go with(next -> n) return(res -> sub_result) group r1;\n");
    let j = as_jump(&s);
    assert_eq!(j.kind, JumpKind::Recurse);
    assert_eq!(j.args[0].expr.text, "next");
    assert_eq!(j.args[0].key.as_deref(), Some("n"));
    assert_eq!(j.ret_map.len(), 1);
    assert_eq!(j.ret_map[0].0.name, "res");
    assert!(j.ret_map[0].0.pkg.is_none());
    assert_eq!(j.ret_map[0].1, "sub_result");
    assert_eq!(j.group.as_deref(), Some("r1"));
}

#[test]
fn jump_sync_is_own_statement() {
    // A3：jump sync 是正式语句
    let b = body("cell fact;\nreceive(n);\njump recurse fact group r1;\njump sync r1;\nexit go with(n);\n");
    assert!(matches!(b[0], Stmt::Jump(_)));
    match &b[1] {
        Stmt::Sync { group, span } => {
            assert_eq!(group, "r1");
            assert_eq!(span.line, 4, "`jump sync r1;` 在第 4 行");
            assert!(span.len > 0);
        }
        other => panic!("期望 Sync，实际 {other:?}"),
    }
}

#[test]
fn jump_and_multiple_targets_read_with_group() {
    // §3.4.2 ④
    let s = only_stmt("cell a;\nreceive(d);\njump and worker, worker when true read with(d) group g1;\n");
    let j = as_jump(&s);
    assert_eq!(j.kind, JumpKind::And);
    assert_eq!(j.targets, vec!["worker".to_string(), "worker".to_string()]);
    assert_eq!(j.condition.as_ref().unwrap().text, "true");
    assert_eq!(j.mode, Some(TransMode::ReadWith));
    assert_eq!(j.group.as_deref(), Some("g1"));
}

#[test]
fn force_modes() {
    let a = only_stmt("cell a;\nreceive(x);\njump t force go with(x -> x);\n");
    assert_eq!(as_jump(&a).mode, Some(TransMode::ForceGoWith));
    let b = only_stmt("cell a;\nreceive(x);\njump t force read with(x -> x);\n");
    assert_eq!(as_jump(&b).mode, Some(TransMode::ForceReadWith));
}

#[test]
fn jump_over_pipeline_and_concurrent_limit() {
    // §3.4.2 ⑤ / 附录 A.3
    let s = only_stmt(
        "cell main;\nreceive();\njump and worker over file_list |> filter(|f| f.size > 1024) |> map(|f| f.path) concurrent_limit=32 group g1;\n",
    );
    let j = as_jump(&s);
    assert_eq!(j.kind, JumpKind::And);
    let over = j.over.as_ref().expect("over");
    assert_eq!(over.collection.text, "file_list");
    assert_eq!(over.ops.len(), 2);
    match &over.ops[0] {
        OverOp::Filter(f) => assert_eq!(f.text, "|f| f.size > 1024"),
        other => panic!("期望 Filter，实际 {other:?}"),
    }
    match &over.ops[1] {
        OverOp::Map(m) => assert_eq!(m.text, "|f| f.path"),
        other => panic!("期望 Map，实际 {other:?}"),
    }
    assert_eq!(j.concurrent_limit.as_ref().unwrap().text, "32");
    assert_eq!(j.group.as_deref(), Some("g1"));
}

#[test]
fn jump_over_all_operators_and_dynamic_limit() {
    let s = only_stmt(
        "cell main;\nreceive(n);\njump and w over xs |> flatmap(|x| x.parts) |> take(10) |> skip(2) concurrent_limit=env_limit group g;\n",
    );
    let j = as_jump(&s);
    let over = j.over.as_ref().expect("over");
    assert!(matches!(over.ops[0], OverOp::FlatMap(_)));
    assert!(matches!(over.ops[1], OverOp::Take(_)));
    assert!(matches!(over.ops[2], OverOp::Skip(_)));
    match &over.ops[1] {
        OverOp::Take(t) => assert_eq!(t.text, "10"),
        other => panic!("期望 Take，实际 {other:?}"),
    }
    assert_eq!(j.concurrent_limit.as_ref().unwrap().text, "env_limit");
}

#[test]
fn jump_segment_order_is_fixed_but_optional() {
    // 只有 when + group 的组合也合法
    let s = only_stmt("cell a;\nreceive(x);\njump t when x > 1 group g;\n");
    let j = as_jump(&s);
    assert!(j.mode.is_none());
    assert!(j.args.is_empty());
    assert!(j.ret_map.is_empty());
    assert!(j.over.is_none());
    assert!(j.concurrent_limit.is_none());
    assert_eq!(j.group.as_deref(), Some("g"));
    // 全部段都缺省
    let s2 = only_stmt("cell a;\nreceive(x);\njump t;\n");
    let j2 = as_jump(&s2);
    assert_eq!(j2.kind, JumpKind::Plain);
    assert!(j2.condition.is_none() && j2.group.is_none());
}

// ------------------------------------------------------- exit / die / out

#[test]
fn exit_go_with_forms() {
    // §3.4.3
    let s = only_stmt("cell a;\nreceive(r);\nexit go with(result);\n");
    match s {
        Stmt::Exit { args, .. } => {
            assert_eq!(args.len(), 1);
            assert_eq!(args[0].expr.text, "result");
            assert_eq!(args[0].key, None);
        }
        other => panic!("期望 Exit，实际 {other:?}"),
    }
    let s = only_stmt("cell a;\nreceive(d, n);\nexit go with(d -> dist, n -> name);\n");
    match s {
        Stmt::Exit { args, .. } => {
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].key.as_deref(), Some("dist"));
            assert_eq!(args[1].key.as_deref(), Some("name"));
        }
        other => panic!("期望 Exit，实际 {other:?}"),
    }
    let s = only_stmt("cell a;\nreceive();\nexit go with();\n");
    match s {
        Stmt::Exit { args, .. } => assert!(args.is_empty()),
        other => panic!("期望 Exit，实际 {other:?}"),
    }
}

#[test]
fn die_and_out() {
    let s = only_stmt("cell a;\nreceive(a1, a2);\ndie;\n");
    match s {
        Stmt::Die { span } => assert!(span.len >= 4),
        other => panic!("期望 Die，实际 {other:?}"),
    }
    let s = only_stmt("cell a;\nreceive(a1, a2);\nout(a1, a2);\n");
    match s {
        Stmt::Out { args, span } => {
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].text, "a1");
            assert_eq!(args[1].text, "a2");
            assert!(span.len > 0);
        }
        other => panic!("期望 Out，实际 {other:?}"),
    }
    // 字符串参数
    let s = only_stmt("cell a;\nreceive();\nout(\"group g1 done\");\n");
    match s {
        Stmt::Out { args, .. } => assert_eq!(args[0].text, "\"group g1 done\""),
        other => panic!("期望 Out，实际 {other:?}"),
    }
}

// ------------------------------------------------------------ extern rust

#[test]
fn extern_rust_global_forms() {
    // §3.4.4 全局
    let p = ok("extern rust {\n    use std::sync::Mutex;\n    fn raw_calc(a: i64) -> i64 { a * a + 1 }\n};\ncell a;\nreceive();\ndie;\n");
    let g: Vec<&RawRust> = p.global_rust().collect();
    assert_eq!(g.len(), 1);
    assert!(g[0].text.starts_with("extern rust {"));
    assert!(g[0].text.contains("fn raw_calc"));
    assert!(!g[0].text.ends_with(';'), "块语句 text 不含尾随分号（A1）");
    assert_eq!(p.cell("a").unwrap().body.len(), 1);

    // read 形式
    let p2 = ok("extern rust read {\n    fn f() {}\n};\ncell a;\nreceive();\ndie;\n");
    let g2: Vec<&RawRust> = p2.global_rust().collect();
    assert!(g2[0].text.starts_with("extern rust read {"));
}

#[test]
fn extern_rust_block_inside_cell() {
    let s = only_stmt("cell hot;\nreceive(a, b) as (x, y);\nextern rust {\n    let temp = x * x + y * y;\n};\n");
    match s {
        Stmt::ExternRustBlock { read_only, body, span } => {
            assert!(!read_only);
            assert_eq!(body.text, "let temp = x * x + y * y;", "body 是花括号内部的逐字切片（两端 trim）");
            assert_eq!(span.line, 3);
        }
        other => panic!("期望 ExternRustBlock，实际 {other:?}"),
    }
    let s = only_stmt("cell hot;\nreceive(x);\nextern rust read { let y = x; };\n");
    match s {
        Stmt::ExternRustBlock { read_only, body, .. } => {
            assert!(read_only);
            assert_eq!(body.text, "let y = x;");
        }
        other => panic!("期望 ExternRustBlock，实际 {other:?}"),
    }
}

// ------------------------------------------------------- 顶层 Rust item

#[test]
fn top_level_rust_items_are_global_passthrough() {
    // §3.5.4 十个关键字全覆盖
    let src = "\
use std::collections::HashMap;
struct Order { id: u64, amount: f64 }
enum E { A, B }
fn helper() -> u64 { 1 }
trait T { fn m(&self); }
impl Order { fn total(&self) -> f64 { self.amount } }
const K: u64 = 7;
static S: u64 = 9;
type Alias = HashMap<u64, Order>;
mod m { pub fn inner() {} }
cell a;
receive();
jump helper;
";
    let p = ok(src);
    let g: Vec<&str> = p.global_rust().map(|r| r.text.as_str()).collect();
    assert_eq!(g.len(), 10);
    assert_eq!(g[0], "use std::collections::HashMap;");
    assert_eq!(g[1], "struct Order { id: u64, amount: f64 }");
    assert_eq!(g[2], "enum E { A, B }");
    assert_eq!(g[3], "fn helper() -> u64 { 1 }");
    assert_eq!(g[4], "trait T { fn m(&self); }");
    assert_eq!(g[5], "impl Order { fn total(&self) -> f64 { self.amount } }");
    assert_eq!(g[6], "const K: u64 = 7;");
    assert_eq!(g[7], "static S: u64 = 9;");
    assert_eq!(g[8], "type Alias = HashMap<u64, Order>;");
    assert_eq!(g[9], "mod m { pub fn inner() {} }");
    assert_eq!(p.cell("a").unwrap().body.len(), 1);
}

#[test]
fn top_level_item_with_attribute() {
    let p = ok("#[derive(Debug)]\nstruct Foo { a: u8 }\ncell a;\nreceive();\ndie;\n");
    let g: Vec<&str> = p.global_rust().map(|r| r.text.as_str()).collect();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0], "#[derive(Debug)]\nstruct Foo { a: u8 }");
}

// ----------------------------------------------------------- Cell 边界

#[test]
fn cell_boundary_two_cells_in_a_row() {
    let p = ok("cell a;\nreceive(x);\nexit go with(x);\ncell b;\nreceive(y);\ndie;\n");
    assert_eq!(p.cells().count(), 2);
    let a = p.cell("a").unwrap();
    assert_eq!(a.body.len(), 1);
    assert_eq!(a.receive.keys[0].name, "x");
    let b = p.cell("b").unwrap();
    assert_eq!(b.body.len(), 1);
    assert!(matches!(b.body[0], Stmt::Die { .. }));
}

#[test]
fn cell_then_top_level_use_is_nested_item_per_a2() {
    // A2 裁定：cell body 只在 cell / entry / extern rust 处结束，因此 cell 之后的
    // Rust item 关键字属于**本 cell 的嵌套 item**（Rust 函数体内 `use` 同样是嵌套项）。
    // 顶层 Rust item 必须写在任何 cell 之前，或包在 `extern rust { .. }` 里。
    let p = ok("cell a;\nreceive(x);\ndie;\nuse std::io;\ncell b;\nreceive();\ndie;\n");
    let a = p.cell("a").expect("cell a");
    assert_eq!(a.body.len(), 2, "use 归入 cell a 的嵌套 item");
    assert_eq!(raw_text(&a.body[1]), "use std::io;");
    assert_eq!(p.global_rust().count(), 0);
    assert_eq!(p.cells().count(), 2);
}

#[test]
fn top_level_rust_item_before_any_cell_is_global() {
    // §3.5.4：顶层（不在 cell 内）遇 Rust item 关键字仍按全局透传
    let p = ok("use std::io;\nstruct S { a: u8 }\ncell a;\nreceive();\ndie;\n");
    assert_eq!(p.global_rust().count(), 2);
    assert_eq!(p.cells().count(), 1);
    assert_eq!(p.cell("a").unwrap().body.len(), 1);
}

#[test]
fn nested_rust_item_stays_in_cell() {
    // A2 裁定：cell 内的 fn helper 属于本 cell（§8.2 字面规则会静默截断 Cell）
    let p = ok("cell a;\nreceive(x);\nfn helper() -> u64 { x }\nlet y = helper();\nexit go with(y);\n");
    let a = p.cell("a").unwrap();
    assert_eq!(a.body.len(), 3);
    assert_eq!(raw_text(&a.body[0]), "fn helper() -> u64 { x }");
    assert_eq!(raw_text(&a.body[1]), "let y = helper();");
    assert!(matches!(a.body[2], Stmt::Exit { .. }));
}

#[test]
fn cell_with_extern_rust_block_and_item() {
    let p = ok("cell a;\nreceive(x);\nextern rust { let t = x; }\nstruct S { a: u8 }\ndie;\n");
    let a = p.cell("a").unwrap();
    assert_eq!(a.body.len(), 3);
    assert!(matches!(a.body[0], Stmt::ExternRustBlock { .. }));
    assert_eq!(raw_text(&a.body[1]), "struct S { a: u8 }");
    assert!(matches!(a.body[2], Stmt::Die { .. }));
}

// --------------------------------------------------------------- 诊断矩阵

#[test]
fn error_e030_unclosed_string() {
    assert_eq!(codes("cell a;\nreceive();\nlet s = \"abc;\ndie;\n"), vec![code::E030]);
}

#[test]
fn error_e030_unclosed_block_comment() {
    assert_eq!(codes("cell a;\nreceive();\n/* 未闭合\ndie;\n"), vec![code::E030]);
}

#[test]
fn error_e030_unbalanced_braces() {
    assert_eq!(codes("cell a;\nreceive();\nif c {\n  die;\n"), vec![code::E030]);
}

#[test]
fn error_e030_unbalanced_parens() {
    assert_eq!(codes("cell a;\nreceive();\nlet s = f(;\n"), vec![code::E030]);
}

#[test]
fn error_e031_orphan_keyword() {
    assert_eq!(codes("cell a;\nreceive();\ngo with(1);\n"), vec![code::E031]);
    assert_eq!(codes("cell a;\nreceive();\nwith(1);\n"), vec![code::E031]);
    assert_eq!(codes("cell a;\nreceive();\nexit(1);\n"), vec![code::E031]);
}

#[test]
fn error_e032_bad_top_level() {
    assert_eq!(codes("123;\n"), vec![code::E032]);
    assert_eq!(codes("foo\n"), vec![code::E032]);
    // DSL 语句出现在顶层（cell 之外的 `die;`）
    assert_eq!(codes("die;\n"), vec![code::E032]);
    // 顶层 `jump`：A2 裁定下 cell body 只在 cell/entry/extern rust 处结束，
    // 所以没有 cell 时的 `jump b;` 是顶层非法 item
    assert_eq!(codes("jump b;\n"), vec![code::E032]);
}

#[test]
fn error_e033_jump_failures() {
    // jump 后缺目标
    assert_eq!(codes("cell a;\nreceive();\njump;\n"), vec![code::E033]);
    // 缺少结尾分号
    assert_eq!(codes("cell a;\nreceive();\njump b\n"), vec![code::E033]);
    // 多余 token
    assert_eq!(codes("cell a;\nreceive();\njump b 1 2;\n"), vec![code::E033]);
    // group 后缺名
    assert_eq!(codes("cell a;\nreceive();\njump and b group;\n"), vec![code::E033]);
}

/// 段序写错必须**点名是哪一段**（规范 §7.4，docs/05 P1-1）。
///
/// 这条钉的是 P1-1 的整条缺陷链：`read with` 写在 `over` 之后时，旧 parser 不报错，
/// 而是把 `read with(payload)` 当成集合表达式吞掉 → 生成 `items read with(payload)
/// .into_iter()` 这种非法 Rust，而 `rscel check` 只剩一条误导的 RSC-E042
/// （"over 只能供给 1 个实参"，用户明明写了 `read with`）。
#[test]
fn error_e033_names_the_misplaced_segment() {
    let src =
        "cell a;\nreceive();\njump and b over items read with(payload) concurrent_limit = 2 group g1;\n";
    let out = parse_program(src, 7);
    assert_eq!(
        out.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec![code::E033],
        "错位段只该给一条 E033，实际: {:?}",
        out.diagnostics.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let msg = &out.diagnostics[0].message;
    assert!(msg.contains("`read with`"), "消息要点名错位的段，实际: {msg}");
    assert!(msg.contains("段序固定"), "消息要给出正确段序，实际: {msg}");
}

#[test]
fn error_e033_names_return_and_swapped_tail_segments() {
    // `return(...)` 属于 mode 段之后的段，写在 over 之后同样要点名
    assert_eq!(codes("cell a;\nreceive();\njump b over items return(x -> y);\n"), vec![code::E033]);
    // `concurrent_limit` 写在 `group` 之后（段序尾部互换）
    assert_eq!(
        codes("cell a;\nreceive();\njump and b over items group g1 concurrent_limit = 2;\n"),
        vec![code::E033]
    );
}

#[test]
fn over_stop_words_keep_canonical_order_and_word_boundaries() {
    // 规范写法（§7.4 给的正面样例）：mode 段在 over **之前**，新增的停止词不能读坏它。
    let s = only_stmt(
        "cell a;\nreceive();\njump and b read with(cfg) over items |> take(2) concurrent_limit = 8 group g1;\n",
    );
    let j = as_jump(&s);
    assert_eq!(j.mode, Some(TransMode::ReadWith));
    assert_eq!(j.args[0].expr.text, "cfg");
    let over = j.over.as_ref().expect("over");
    assert_eq!(over.collection.text, "items");
    assert!(matches!(over.ops[0], OverOp::Take(_)));
    assert_eq!(j.concurrent_limit.as_ref().unwrap().text, "8");
    assert_eq!(j.group.as_deref(), Some("g1"));

    // 词边界：标识符里含 `return` / `group` 前缀不算段关键字（`match_word` 看词尾）。
    let s2 = only_stmt("cell a;\nreceive();\njump and b over returned_items |> take(grouped);\n");
    let j2 = as_jump(&s2);
    assert_eq!(j2.over.as_ref().expect("over").collection.text, "returned_items");
}

#[test]
fn error_e034_receive_missing() {
    // §3.4.1 规则 1
    assert_eq!(codes("cell a;\ndie;\n"), vec![code::E034]);
    assert_eq!(codes("cell a;\nlet x = 1;\n"), vec![code::E034]);
}

#[test]
fn error_e036_duplicate_cell() {
    // A6 扩展码
    assert_eq!(codes("cell a;\nreceive();\ndie;\ncell a;\nreceive();\ndie;\n"), vec![code::E036]);
}

#[test]
fn error_e037_multiple_entry() {
    // A4 / A6 扩展码
    assert_eq!(codes("entry a;\nentry b;\ncell a;\nreceive();\ndie;\n"), vec![code::E037]);
    // 只有一条时不报
    assert!(parse_program("entry a;\ncell a;\nreceive();\ndie;\n", 0).diagnostics.is_empty());
}

#[test]
fn error_diagnostic_carries_cell_name_and_position() {
    let out = parse_program("cell fact;\nreceive(n);\ngo with(1);\n", 3);
    let d = out.first_error().expect("有错误");
    assert_eq!(d.code, code::E031);
    assert_eq!(d.cell.as_deref(), Some("fact"));
    assert_eq!(d.span.line, 3);
    assert_eq!(d.span.file, 3);
    let text = d.render("fact.rscel");
    assert!(text.starts_with("RSC-E031: 孤立关键字"), "{text}");
    assert!(text.contains("位置: cell fact @ 3:1"), "{text}");
}

#[test]
fn parser_keeps_going_after_error() {
    // 出错后仍应尽量产出已解析部分
    let out = parse_program("cell a;\nreceive(x);\ngo with(1);\njump b go with(x);\ncell c;\nreceive();\ndie;\n", 0);
    assert!(out.has_errors());
    assert_eq!(out.program.cells().count(), 2, "错误之后的 cell 仍被解析");
    let a = out.program.cell("a").expect("cell a");
    assert_eq!(a.body.len(), 1, "孤立关键字被丢弃，其余语句保留");
    assert!(matches!(a.body[0], Stmt::Jump(_)));
}

#[test]
fn empty_and_whitespace_only_source() {
    let out = parse_program("", 0);
    assert!(!out.has_errors());
    assert!(out.program.items.is_empty());
    let out = parse_program("   \n\t // 只有注释\n /* 块 */ ", 0);
    assert!(!out.has_errors());
    assert!(out.program.items.is_empty());
}

#[test]
fn spans_point_at_source() {
    let src = "cell a;\nreceive(x);\nlet y = 1;\n";
    let c = first_cell(src);
    assert_eq!(c.span.line, 1);
    assert_eq!(c.receive.span.line, 2);
    match &c.body[0] {
        Stmt::RawRust(r) => {
            assert_eq!((r.span.line, r.span.col), (3, 1));
            assert_eq!(r.span.len as usize, "let y = 1;".len());
        }
        other => panic!("期望 RawRust，实际 {other:?}"),
    }
}

// ------------------------------------------------- 附录 A：完整语法示例集

#[test]
fn appendix_a1_factorial_recurse() {
    let c = first_cell(
        "cell fact;\n\
         receive(n) as (num);\n\
         let mut sub_result: Option<i64> = None;\n\
         let next = num - 1;\n\
         jump recurse fact when num > 1 go with(next -> n) return(res -> sub_result) group r1;\n\
         jump sync r1;\n\
         let result = num * sub_result.unwrap();\n\
         exit go with(result);\n",
    );
    assert_eq!(c.name, "fact");
    assert_eq!(c.receive.aliases, vec!["num".to_string()]);
    assert_eq!(c.body.len(), 6);
    match &c.body[2] {
        Stmt::Jump(j) => {
            assert_eq!(j.kind, JumpKind::Recurse);
            assert_eq!(j.args[0].key.as_deref(), Some("n"));
            assert_eq!(j.ret_map[0].1, "sub_result");
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
    match &c.body[3] {
        Stmt::Sync { group, .. } => assert_eq!(group, "r1"),
        other => panic!("期望 Sync，实际 {other:?}"),
    }
}

#[test]
fn appendix_a2_parallel_worker_with_group_receive() {
    let p = ok(
        "cell parent;\n\
         receive(g1.W1.A, g1.W2.A) as (a1, a2);\n\
         let data = [10, 20];\n\
         jump and worker over data group g1;\n\
         jump sync g1;\n\
         out(a1, a2);\n\
         exit go with();\n\
         \n\
         cell worker;\n\
         receive data;\n\
         let A = data + 1;\n\
         exit go with(A);\n",
    );
    let parent = p.cell("parent").expect("cell parent");
    // §7.2：多源并行必须 `包.变量` 限定，parser 给出 pkg/name 切分结果供 check 层校验
    assert_eq!(parent.receive.keys[0].pkg.as_deref(), Some("g1.W1"));
    assert_eq!(parent.receive.keys[0].name, "A");
    assert_eq!(parent.receive.keys[1].pkg.as_deref(), Some("g1.W2"));
    assert_eq!(parent.receive.aliases, vec!["a1".to_string(), "a2".to_string()]);
    match &parent.body[1] {
        Stmt::Jump(j) => {
            assert_eq!(j.kind, JumpKind::And);
            assert_eq!(j.over.as_ref().unwrap().collection.text, "data");
            assert!(j.over.as_ref().unwrap().ops.is_empty());
            assert_eq!(j.group.as_deref(), Some("g1"));
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
    assert_eq!(p.cell("worker").expect("cell worker").body.len(), 2);
}

#[test]
fn appendix_a3_over_with_filter_map_and_limit() {
    let main = first_cell(
        "cell main;\n\
         receive();\n\
         let file_list = [...];\n\
         jump and file_processor over file_list |> filter(|f| f.size > 1024) |> map(|f| f.path) concurrent_limit=32 group g1;\n\
         jump sync g1;\n\
         exit go with();\n",
    );
    match &main.body[1] {
        Stmt::Jump(j) => {
            let over = j.over.as_ref().expect("over");
            assert_eq!(over.collection.text, "file_list");
            assert_eq!(over.ops.len(), 2);
            assert_eq!(j.concurrent_limit.as_ref().unwrap().text, "32");
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
    // 配套的 file_processor（注释里的 jump 引用不影响）
    let fp = first_cell("cell file_processor;\nreceive path;\n// 读取文件、计算哈希\nexit go with(hash);\n");
    assert_eq!(fp.body.len(), 1);
}

#[test]
fn appendix_a4_mutually_exclusive_branches() {
    let c = first_cell(
        "cell branch_test;\n\
         receive x;\n\
         jump cell_1 when x == 1 go with(x);\n\
         jump cell_2 when x == 2 go with(x);\n\
         jump cell_3 when x == 3 go with(x);\n\
         die; // x 为其他值，显式终止\n",
    );
    assert_eq!(c.body.len(), 4);
    for (i, cond) in ["x == 1", "x == 2", "x == 3"].iter().enumerate() {
        match &c.body[i] {
            Stmt::Jump(j) => {
                assert_eq!(j.kind, JumpKind::Plain);
                assert_eq!(j.condition.as_ref().unwrap().text, *cond);
                assert_eq!(j.mode, Some(TransMode::GoWith));
            }
            other => panic!("第 {i} 条期望 Jump，实际 {other:?}"),
        }
    }
}

#[test]
fn appendix_a5_mixed_dsl_and_extern_rust() {
    let p = ok(
        "extern rust {\n\
             fn fast_mul(a: i64, b: i64) -> i64 { a.wrapping_mul(b) }\n\
         };\n\
         \n\
         cell calc;\n\
         receive(a, b) as (x, y);\n\
         extern rust {\n\
             let r = fast_mul(x, y);\n\
         };\n\
         let result = r;\n\
         exit go with(result);\n",
    );
    let g: Vec<&str> = p.global_rust().map(|r| r.text.as_str()).collect();
    assert_eq!(g.len(), 1);
    assert!(g[0].contains("fast_mul"));
    let c = p.cell("calc").expect("cell calc");
    assert_eq!(c.receive.aliases, vec!["x".to_string(), "y".to_string()]);
    assert_eq!(c.body.len(), 3);
    match &c.body[0] {
        Stmt::ExternRustBlock { read_only, body, .. } => {
            assert!(!read_only);
            assert_eq!(body.text, "let r = fast_mul(x, y);");
        }
        other => panic!("期望 ExternRustBlock，实际 {other:?}"),
    }
}

#[test]
fn appendix_a6_multi_level_read_with_forwarding() {
    let p = ok(
        "cell a;\n\
         receive();\n\
         let big_data = \"some large read-only data\";\n\
         jump recurse b when true read with(big_data) group g1;\n\
         jump sync g1;\n\
         out(\"group g1 done\");\n\
         exit go with();\n\
         \n\
         cell b;\n\
         receive big_data;\n\
         jump recurse c when true read with(big_data) group g2;\n\
         jump sync g2;\n\
         exit go with();\n\
         \n\
         cell c;\n\
         receive big_data;\n\
         // 只读使用 big_data\n\
         exit go with();\n",
    );
    assert_eq!(p.cells().count(), 3);
    let a = p.cell("a").expect("cell a");
    match &a.body[1] {
        Stmt::Jump(j) => assert_eq!(j.mode, Some(TransMode::ReadWith)),
        other => panic!("期望 Jump，实际 {other:?}"),
    }
    match &a.body[3] {
        Stmt::Out { args, .. } => assert_eq!(args[0].text, "\"group g1 done\""),
        other => panic!("期望 Out，实际 {other:?}"),
    }
    assert_eq!(p.cell("c").expect("cell c").body.len(), 1);
}

#[test]
fn appendix_1_2_entry_and_argv_program() {
    // §1.2 示例：entry + `jump ... go with(args -> argv)`
    let p = ok(
        "entry start_flow;\n\
         \n\
         cell start_flow;\n\
         receive();\n\
         let args = std::env::args().collect::<Vec<_>>();\n\
         jump process go with(args -> argv);\n",
    );
    assert_eq!(p.entry.as_deref(), Some("start_flow"));
    let c = p.cell("start_flow").expect("cell start_flow");
    match &c.body[1] {
        Stmt::Jump(j) => {
            assert_eq!(j.targets, vec!["process".to_string()]);
            assert_eq!(j.args[0].key.as_deref(), Some("argv"));
            assert_eq!(j.args[0].effective_key(), "argv");
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
}

#[test]
fn count_loop_example_from_language_doc() {
    // 语言设计文档 §5.3
    let c = first_cell(
        "cell count_loop;\n\
         receive i, sum;\n\
         jump loop count_loop when i < 5 go with(i + 1, sum + i);\n\
         exit go with(sum);\n",
    );
    assert_eq!(c.receive.keys.len(), 2);
    assert_eq!(c.receive.aliases, vec!["i".to_string(), "sum".to_string()]);
    match &c.body[0] {
        Stmt::Jump(j) => {
            assert_eq!(j.kind, JumpKind::Loop);
            assert_eq!(j.targets, vec!["count_loop".to_string()]);
            assert_eq!(j.args.len(), 2);
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
}
