//! §12.9 源映射表的**正确性**测试（§18.1 测试策略点名的「srcmap：映射正确性」）。
//!
//! 断言全部对着「生成的 `rust_source` 按行切片」来验，不硬编码任何行号：
//! 行号是 codegen 的产物，写死一个数字等于把一个测试绑在一次重构上；
//! 这里要证明的是「srcmap 说第 N 行属于 DSL 第 M 条语句」这句话**是不是真的**。

use rscel_gen::srcmap::{parse_srcmap_json, MapKind, SrcMap};
use rscel_gen::{generate_as, GeneratedCode};
use rscel_parser::parse_program;

const SRC: &str = include_str!("../examples/factorial_loop.rscel");

/// 解析 → 生成 → 把 `.map.json` 文本读回结构化表，一次拿到三份数据。
fn build(src: &str, file_name: &str) -> (GeneratedCode, SrcMap, Vec<String>) {
    let parsed = parse_program(src, 0);
    assert!(!parsed.has_errors(), "解析应无错，实际: {:#?}", parsed.diagnostics);
    let g = generate_as(&parsed.program, file_name);
    let text = g.source_map.as_deref().expect("generate_as 必须产出 §12.9 的映射表文本");
    let map = parse_srcmap_json(text).expect("自产的 .map.json 必须能读回");
    // 落盘文本 == 读回后再序列化一次：证明序列化器与解析器互验通过（CLI 依赖这条路）。
    assert_eq!(text, map.to_json(), "JSON 往返必须稳定");
    let lines: Vec<String> = g.rust_source.lines().map(|l| l.to_string()).collect();
    (g, map, lines)
}

/// DSL 源里第一条 trim 后等于 `needle` 的行号（1 起始）。
fn dsl_line(src: &str, needle: &str) -> usize {
    src.lines()
        .position(|l| l.trim() == needle)
        .unwrap_or_else(|| panic!("DSL 源里找不到 {needle:?}"))
        + 1
}

/// 生成 Rust 里函数 `fname` 的函数体边界：`(头行号, 收尾右括号行号)`，均 1 起始。
///
/// 为什么能这么找：codegen 的 Cell 函数总是以 `fn cell_x(..) -> Option<RetX> {` 开头、
/// 以**顶格的** `}` 收尾，函数体内所有行都有缩进，所以第一个顶格 `}` 就是边界。
fn fn_body(lines: &[String], fname: &str) -> (usize, usize) {
    let prefix_sync = format!("fn {fname}(");
    let prefix_async = format!("async fn {fname}(");
    let head = lines
        .iter()
        .position(|l| l.starts_with(&prefix_sync) || l.starts_with(&prefix_async))
        .unwrap_or_else(|| panic!("生成物里没有 {fname}"));
    let close_rel = lines[head + 1..]
        .iter()
        .position(|l| l == "}")
        .unwrap_or_else(|| panic!("{fname} 没有顶格收尾的 }}"));
    // `head` 是 0 基下标、`head + 1` 是 1 基头行号；收尾行的 0 基下标是 `head + 1 + close_rel`，
    // 换成 1 基就是 `head + 2 + close_rel`（这个 ±1 错过一次：区间末行被当成函数尾）。
    (head + 1, head + 2 + close_rel)
}

// ------------------------------------------------------------------ (a) cells 表

#[test]
fn cells_table_lists_every_cell_with_its_dsl_definition_position() {
    let (_g, map, _lines) = build(SRC, "factorial_loop.rs");
    assert_eq!(map.version, 1, "§12.9 要求 version == 1");
    assert_eq!(map.file, "factorial_loop.rs", "file 字段是调用方传进来的生成文件名");

    let names: Vec<&str> = map.cells.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"fact"), "cells 应有 fact，实际 {names:?}");
    assert!(names.contains(&"main"), "cells 应有 main，实际 {names:?}");

    // 位置必须是 DSL 源里 `cell fact;` 那一行（列 1）—— 这验证了「映射到 DSL 侧」而不是
    // 「映射到生成侧」，两者都是行号，混了就全错。
    assert_eq!(map.cell_pos("fact").map(|p| (p.line, p.col)), Some((dsl_line(SRC, "cell fact;") as u32, 1)));
    assert_eq!(map.cell_pos("main").map(|p| (p.line, p.col)), Some((dsl_line(SRC, "cell main;") as u32, 1)));
}

// ------------------------------------------------------------------ (b) 区间真的落在函数体里

/// `cell fact` 只有 main 一个 plain 调用点 → 被**实体展开**进 `fn cell_main`（候选 1，
/// 数据见 `docs/06-控制变量实验.md`）。所以它的语句区间现在落在**调用方**的函数体里，
/// 而不是那段已经不存在的 `fn cell_fact`。
///
/// 这条取代展开前的「落在 cell_fact 体内」：抓错边的手法（区间里每一行都必须是非空
/// 代码行）原样保留 —— 展开改的是行住在哪个函数里，不是行号数得对不对。
#[test]
fn expanded_cell_ranges_lie_inside_the_caller_body() {
    let (_g, map, lines) = build(SRC, "factorial_loop.rs");
    assert!(
        !lines.iter().any(|l| l.starts_with("fn cell_fact(")),
        "被展开的 Cell 不该再留独立函数"
    );
    let (head, close) = fn_body(&lines, "cell_main");

    let stmts: Vec<_> = map.entries.iter().filter(|e| e.kind == MapKind::Stmt && e.dsl.cell == "fact").collect();
    assert!(
        stmts.len() >= 3,
        "fact 的每条语句都该有自己的映射条目（展开后仍然逐条记，不能被调用方那条 jump 吞掉），实际 {stmts:?}"
    );
    for e in &stmts {
        assert!(e.dsl.line > 0, "语句映射必须带 DSL 行号: {e:?}");
        assert!(
            e.rs_line_start > head && e.rs_line_end < close,
            "映射区间 {}..={} 必须落在展开它的 cell_main 体内（{head}..{close}）",
            e.rs_line_start,
            e.rs_line_end
        );
        // 区间里的每一行都必须是非空代码行 —— 若边界数错，这里会立刻抓到空白/下一个函数。
        for ln in e.rs_line_start..=e.rs_line_end {
            assert!(!lines[ln - 1].trim().is_empty(), "第 {ln} 行是空行，说明区间越界了");
        }
    }
}

// ------------------------------------------------------------------ (c) 全局形状不变式

// ------------------------------------------------------------------ (c) 全局形状不变式

/// 展开**两份**副本之后，§12.9 的表仍然「升序 + 互不重叠」，而且每条 DSL 语句有几个
/// 落点就在表里出现几次，`lookup` 按落点行各自命中。
///
/// 为什么单独测：展开的映射保真度此前只在「单一调用点」的形状上验过（`factorial_loop`
/// 的 fact 只有一份副本）。多调用点展开（`docs/05` P0-5 第 2 项，数据在 `docs/06` 实验七）
/// 让**同一个 DSL 行**对应两段不同的生成行区间 —— 这是合法形状（一条语句有两个落点），
/// 但它同时考验三件事：`SrcMapBuilder::push` 的有序插入、区间边界数得对不对、
/// 以及编辑器/诊断按行反查时拿到的必须是**它自己那一份**。
#[test]
fn two_expanded_copies_keep_the_map_sorted_and_lookupable() {
    const SRC2: &str = "\
entry main;

cell main;
receive(mode: i64);
jump t when mode == 1i64 go with(10i64 -> v);
jump t go with(20i64 -> v);

cell t;
receive(v: i64);
out(v);
exit go with();
";
    let (_g, map, lines) = build(SRC2, "two_copies.rs");
    assert!(
        !lines.iter().any(|l| l.starts_with("fn cell_t(")),
        "两个 plain 调用点都要展开，不该留 `fn cell_t`"
    );

    let mut prev_end = 0usize;
    for e in &map.entries {
        assert!(e.rs_line_start <= e.rs_line_end, "区间不能倒挂: {e:?}");
        assert!(
            e.rs_line_start > prev_end,
            "条目必须升序且互不重叠：{}..={} 与上一条的 {prev_end} 冲突",
            e.rs_line_start,
            e.rs_line_end
        );
        prev_end = e.rs_line_end;
    }

    // DSL 里 `out(v);` 只有一行，生成物里有两个落点（每个调用点一份）。
    let dsl_out = dsl_line(SRC2, "out(v);") as u32;
    let hits: Vec<&rscel_gen::srcmap::MapEntry> = map
        .entries
        .iter()
        .filter(|e| e.kind == MapKind::Stmt && e.dsl.cell == "t" && e.dsl.line == dsl_out)
        .collect();
    assert_eq!(hits.len(), 2, "`out(v);` 应有两处生成落点，实际 {hits:?}");
    for e in &hits {
        // 边界数错的探测器：区间内每一行都必须是非空代码行。
        for ln in e.rs_line_start..=e.rs_line_end {
            assert!(!lines[ln - 1].trim().is_empty(), "第 {ln} 行是空行，说明区间越界了");
        }
        let got = map.lookup(e.rs_line_start).expect("落点行必须查得到");
        assert_eq!(
            (got.dsl.cell.as_str(), got.dsl.line),
            ("t", dsl_out),
            "lookup 要返回这一份副本自己的 DSL 语句，而不是另一份"
        );
    }
    // 两份副本的行区间不许重叠（重叠 = 后抄的那份把前一份的行号覆盖掉了）。
    assert!(
        hits[0].rs_line_end < hits[1].rs_line_start,
        "两处落点必须各占一段：{:?} vs {:?}",
        hits[0],
        hits[1]
    );
}

#[test]
fn entries_are_sorted_non_overlapping_and_cover_cell_bodies() {
    let (_g, map, lines) = build(SRC, "factorial_loop.rs");
    assert!(map.runtime_lines > 0, "必须记下运行时头部的行数，才能界定未映射区间");

    let mut prev_end = 0usize;
    for e in &map.entries {
        assert!(e.rs_line_start <= e.rs_line_end, "区间不能倒挂: {e:?}");
        assert!(
            e.rs_line_start > prev_end,
            "条目必须互不重叠（升序）：{}..={} 与上一条的 {prev_end} 冲突",
            e.rs_line_start,
            e.rs_line_end
        );
        assert!(
            e.rs_line_start > map.runtime_lines,
            "运行时头部（{} 行）之后的内容才可能有 DSL 对应物",
            map.runtime_lines
        );
        assert!(e.rs_col == 1, "生成区间的左边界按列 1 起算（缩进不是语义列）");
        prev_end = e.rs_line_end;
    }

    // 「覆盖函数主体」：cell_main 的函数体里，每个非空行都必须被某条映射接管。
    // 接管者的 `dsl.cell` 允许是 **main 或 fact** —— fact 被实体展开进 main 的唯一
    // plain 调用点（候选 1），它的语句仍逐条记在 fact 名下，这正是展开后必须保住的
    // 映射保真度（第一版把展开体抄进临时缓冲，`out` 语句的条目就是这样丢的）。
    let (head, close) = fn_body(&lines, "cell_main");
    for ln in head + 1..close {
        if lines[ln - 1].trim().is_empty() {
            continue;
        }
        let hit = map
            .entries
            .iter()
            .any(|e| e.covers(ln) && matches!(e.dsl.cell.as_str(), "main" | "fact"));
        assert!(hit, "第 {ln} 行（cell_main 体内）没有被任何映射接管: {:?}", lines[ln - 1]);
    }

    // 严格版（每行必须由**它自己 Cell** 的映射接管）改用一个没有展开的样例跑：
    // `jump_and_worker` 的 worker 是 `jump and` 派生的，不是 plain 调用点，不展开。
    let (_g2, map2, lines2) = build(
        include_str!("../examples/jump_and_worker.rscel"),
        "jump_and_worker.rs",
    );
    for cell in ["start", "worker"] {
        let (h, c) = fn_body(&lines2, &format!("cell_{cell}"));
        for ln in h + 1..c {
            if lines2[ln - 1].trim().is_empty() {
                continue;
            }
            let hit = map2
                .entries
                .iter()
                .any(|e| e.covers(ln) && e.dsl.cell == cell);
            assert!(
                hit,
                "jump_and_worker 第 {ln} 行（{cell} 体内）没有被 {cell} 的映射覆盖: {:?}",
                lines2[ln - 1]
            );
        }
    }
}

// ------------------------------------------------------------------ (d) 行号一致性 = 映射可用

/// 「映射可用」的证据：拿 DSL 语句 `out(next_acc);` 去查表，
/// 按表切出来的生成行必须真的是 `println!`。
#[test]
fn out_statement_maps_onto_generated_println_line() {
    let (_g, map, lines) = build(SRC, "factorial_loop.rs");
    let dsl = dsl_line(SRC, "out(next_acc);");
    let e = map
        .entries
        .iter()
        .find(|e| e.kind == MapKind::Stmt && e.dsl.line as usize == dsl)
        .unwrap_or_else(|| panic!("没有 DSL 第 {dsl} 行的映射条目"));
    assert_eq!(e.dsl.cell, "fact");
    let body: Vec<&str> = lines[e.rs_line_start - 1..e.rs_line_end].iter().map(|s| s.as_str()).collect();
    assert!(
        body.iter().any(|l| l.contains("println!") && l.contains("next_acc")),
        "out(next_acc) 的生成区间应含 println!，实际: {body:?}"
    );
}

/// 同一条思路再验一句：`let five: i64 = 5;` 必须原样出现在它自己的区间里。
/// 这条能抓住「RawRust 透传的行被算进了别的语句区间」。
#[test]
fn let_statement_maps_onto_its_own_generated_line() {
    let (_g, map, lines) = build(SRC, "factorial_loop.rs");
    let dsl = dsl_line(SRC, "let five: i64 = 5;");
    let e = map
        .entries
        .iter()
        .find(|e| e.kind == MapKind::Stmt && e.dsl.line as usize == dsl)
        .expect("main 的 let 应有映射");
    assert_eq!(e.dsl.cell, "main");
    assert!(
        lines[e.rs_line_start - 1..e.rs_line_end].iter().any(|l| l.trim() == "let five: i64 = 5;"),
        "let 透传应逐字落在自己的区间里"
    );
}

/// `jump loop` 展开成多行（`let mut` 声明 + `loop { ... }`），
/// 验证「一条 DSL 语句 ↔ 一段多行区间」而不是只记首行。
#[test]
fn jump_loop_maps_onto_a_multiline_range() {
    let (_g, map, lines) = build(SRC, "factorial_loop.rs");
    let dsl = dsl_line(SRC, "jump loop fact when next > 0 go with(next, next_acc);");
    let e = map.entries.iter().find(|e| e.dsl.line as usize == dsl).expect("jump loop 应有映射");
    assert!(e.rs_line_end > e.rs_line_start, "loop 展开必然多行，实到 {}..={}", e.rs_line_start, e.rs_line_end);
    let seg = &lines[e.rs_line_start - 1..e.rs_line_end];
    assert!(seg.iter().any(|l| l.trim_start().starts_with("loop {")), "区间里应看到 loop 块: {seg:?}");
    assert!(seg.iter().any(|l| l.contains("break")), "区间里应看到条件 break: {seg:?}");
}

// ------------------------------------------------------------------ 全样例结构不变式

/// 对所有仓库样例做结构检查：区间不倒挂、不重叠、不越界，
/// 且 `runtime_lines` 之后的行才允许有映射。
///
/// 为什么不只验 `factorial_loop`：instrumentation 的风险点是
/// 「一条语句展开成很多行 / 展开顺序被打乱」的形态（`jump and` 组派生、`jump sync`
/// 解包、`over` 链、`extern rust`），factorial 恰好一个都不碰。
/// 这几个样例跑完，插桩在真实多行展开上成立的概率才有凭据。
#[test]
fn structural_invariants_hold_for_every_example() {
    // 第三列 = 「这个样例应当产出可映射的代码吗」。
    // `extern_rust_mix.rscel`（附录 A.6）以前是 `false`：它的 `fast_mul(a, b)` 推不出类型
    // 而只发 `compile_error!`。P0-2 把「调用点可见的 fn 签名返回类型」补成 D2 的第四档
    // 来源以后，它和其它样例一样产出可映射代码。
    let cases: &[(&str, &str, bool)] = &[
        ("factorial_loop.rs", include_str!("../examples/factorial_loop.rscel"), true),
        ("jump_and_worker.rs", include_str!("../examples/jump_and_worker.rscel"), true),
        ("multi_branch_die.rs", include_str!("../../../examples/multi_branch_die.rscel"), true),
        ("extern_rust_mix.rs", include_str!("../../../examples/extern_rust_mix.rscel"), true),
    ];
    for (file, src, expect_entries) in cases {
        let (_g, map, lines) = build(src, file);
        assert_eq!(map.version, 1, "{file}");
        assert_eq!(!map.entries.is_empty(), *expect_entries, "{file}: entries 是否符合预期");
        let mut prev_end = 0usize;
        for e in &map.entries {
            assert!(e.rs_line_start <= e.rs_line_end, "{file}: 区间倒挂 {e:?}");
            assert!(e.rs_line_start > prev_end, "{file}: 条目重叠 {e:?}（上一条止于 {prev_end}）");
            assert!(e.rs_line_end <= lines.len(), "{file}: 区间越过生成文件行数 {e:?}");
            assert!(e.rs_line_start > map.runtime_lines, "{file}: 映射指到了运行时头部 {e:?}");
            // extern 只可能出现在 extern rust 语句 / 顶层 Rust item 上。
            assert!(
                !e.in_extern || matches!(e.kind, MapKind::Stmt | MapKind::GlobalExtern),
                "{file}: {e:?} 标了 in_extern 但 kind 不可能是用户 Rust"
            );
            // 语句级区间**绝不**能吞掉一个顶层 item 或函数的收尾括号：
            // 一旦出现顶格的 `}` / `fn ` / `struct `，就说明「emit_stmt 前后数行数」的
            // 边界算错了（少了一次换行、或把 Cell 尾算进了最后一条语句）。
            if e.kind == MapKind::Stmt {
                for ln in e.rs_line_start..=e.rs_line_end {
                    let l = &lines[ln - 1];
                    assert!(
                        !(l.starts_with('}') || l.starts_with("fn ") || l.starts_with("async fn ") || l.starts_with("struct ")),
                        "{file}: 语句区间 {}..={} 吞掉了顶层结构: {l:?}",
                        e.rs_line_start,
                        e.rs_line_end
                    );
                }
            }
            prev_end = e.rs_line_end;
        }
        // 每个 Cell 的 DSL 定义都进了 cells 表（errmap 的 `cell <名> @ 行:列` 靠它兜底）。
        for name in map.cells.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>() {
            assert!(map.cell_pos(&name).is_some(), "{file}: cells 表缺 {name}");
        }
    }
}

// ------------------------------------------------------------------ in_extern 标记

/// `extern rust {}`（Cell 内 + 顶层）必须标 `in_extern: true`（§15.2 的 RSC-E901 判据），
/// 而 Cell 体内的普通 Rust 透传必须标 `false` —— 否则最常见的类型写错都拿不到 DSL 位置。
#[test]
fn extern_rust_blocks_are_flagged_and_plain_passthrough_is_not() {
    let src = "\
extern rust {
    fn helper(a: i64) -> i64 { a }
};

cell main;
receive();
extern rust { let y: i64 = helper(1); };
let z: i64 = 2;
exit go with();
";
    let (_g, map, lines) = build(src, "extern_mix.rs");

    let extern_line = dsl_line(src, "extern rust { let y: i64 = helper(1); };");
    let e = map.entries.iter().find(|e| e.dsl.line as usize == extern_line).expect("Cell 内 extern rust 应有映射");
    assert_eq!(e.kind, MapKind::Stmt);
    assert!(e.in_extern, "Cell 内 extern rust 块必须标 in_extern");
    assert!(
        lines[e.rs_line_start - 1..e.rs_line_end].iter().any(|l| l.contains("let y")),
        "extern 块的内容应落在自己的区间里"
    );

    let z_line = dsl_line(src, "let z: i64 = 2;");
    let z = map.entries.iter().find(|e| e.dsl.line as usize == z_line).expect("透传 let 应有映射");
    assert!(!z.in_extern, "Cell 体内的普通透传不是 extern，否则 RSC-E900 就废了");

    let top = map.entries.iter().find(|e| e.kind == MapKind::GlobalExtern).expect("顶层 extern rust 应记一条");
    assert!(top.in_extern, "§3.5.4：顶层 Rust item = 隐含 extern");
    assert!(lines[top.rs_line_start - 1..top.rs_line_end].iter().any(|l| l.contains("fn helper")));
}

/// 类型推不出而只发 `compile_error!` 的那条路径：映射表**仍要产出**，
/// 且诚实地报告「没有任何 DSL 区间」（生成物不是任何语句的翻译）。
///
/// 这里的 DSL 故意绕过 check 层（本测试直接调 `generate`，不跑 check）：
/// `cell w` 的 `receive(x)` 既没有 `: i64` 注解、入边 `jump w go with()` 也没带实参，
/// D2 的全部来源（注解 / 入边实参 / 调用点 fn 签名 / 字面与 let 递归）都落空
/// → codegen 只发 `compile_error!`（规约 §6.1「做不到就报错，不假装成功」）。
#[test]
fn compile_error_path_yields_empty_but_valid_map() {
    let src = "\
cell main;
receive();
jump w go with();

cell w;
receive(x);
exit go with();
";
    let parsed = parse_program(src, 0);
    assert!(!parsed.has_errors());
    let g = rscel_gen::generate(&parsed.program);
    assert!(g.rust_source.contains("compile_error!"), "该 DSL 应走 compile_error! 路径，实际生成:\n{}", g.rust_source);
    let text = g.source_map.as_deref().expect("也要有 map");
    let map = parse_srcmap_json(text).expect("JSON 必须可读回");
    assert!(map.entries.is_empty(), "compile_error! 路径不该声称任何 DSL 区间");
    // 生成物为了消掉 rustc 的 E0601 噪声会补一行 `fn main() {}`（docs/05 P2），
    // 而那行不对应任何 DSL 语句 —— 所以「有 main」和「映射表为空」必须同时成立，
    // 补 main 不许顺手登记条目。
    assert!(
        g.rust_source.contains("fn main()"),
        "该路径应带上 fn main，免得 rustc 附带无关的 E0601，实际:\n{}",
        g.rust_source
    );
    assert!(
        map.entries.is_empty(),
        "补出来的 fn main 不能进映射表，实际有 {} 条",
        map.entries.len()
    );
    assert_eq!(map.version, 1);
    // cells 表仍然填着：Cell 的 DSL 位置与「有没有生成代码」无关，留着它才有诊断价值。
    assert!(map.cells.iter().any(|(n, _)| n == "w"), "cells 表应仍然登记 w");
}

/// 非 `extern rust` 的 Rust 块（`if cond { ... }` 含 DSL 语句）按设计**不**标 extern：
/// 块里生成的行属于 DSL 语义，反映射比 RSC-E901 有用。这条把设计决定钉住，
/// 免得以后有人「顺手把 RustBlock 全标成 in_extern」。
#[test]
fn dsl_inside_rust_block_is_not_marked_extern() {
    let src = "\
cell main;
receive(k: i64);
if k > 0 {
    die;
}
exit go with();
";
    let (_g, map, _lines) = build(src, "block.rs");
    let if_line = dsl_line(src, "if k > 0 {");
    let e = map.entries.iter().find(|e| e.dsl.line as usize == if_line).expect("块语句应有映射");
    assert!(!e.in_extern, "含 DSL 语句（die）的 Rust 块不是 extern 区间");
}
