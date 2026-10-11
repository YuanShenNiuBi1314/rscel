//! §15.3 `--debug` 埋点的测试（终极目标 2「前端专属编辑器 + 拓扑调试」的数据源）。
//!
//! 钉住三件事，每件都对应一条真实的回归风险：
//!
//! 1. **release 一个字节都不许动**（`release_build_has_no_trace_artifact_in_any_shape`、
//!    `debug_output_adds_only_whole_lines_apart_from_the_permit_closure`）。性能门禁比的
//!    就是生成物，埋点若渗进 release 路径，分母会跟着漂。
//! 2. **事件词汇表逐字对得上 DSL**（`worker_group_trace_lines_carry_dsl_positions_and_key_names`
//!    与 `every_trace_line_matches_the_documented_vocabulary`）。位置是 **DSL** 行号，
//!    编辑器拿它才能直接高亮；键名按 DSL 写法，不是内部的 `v_<id>`。
//! 3. **埋点不破坏既有不变式**：§12.9 映射表仍然升序、互不重叠
//!    （`debug_srcmap_keeps_the_sorted_non_overlapping_invariant`），
//!    `extern rust{}` 块内一行都不插（§15.3 原文要求，
//!    `trace_lines_never_appear_inside_extern_rust_ranges`），
//!    `jump loop` 不插（§9.2：它不搬 Packet，且在热循环里每轮一次）。

use rscel_gen::srcmap::{parse_srcmap_json, SrcMap};
use rscel_gen::{generate_as_with, GenOptions};
use rscel_parser::parse_program;
use std::process::Command;

const FACTORIAL: &str = include_str!("../examples/factorial_loop.rscel");
const WORKER: &str = include_str!("../examples/jump_and_worker.rscel");
const EXTERN_MIX: &str = include_str!("../../../examples/extern_rust_mix.rscel");
const MULTI_DIE: &str = include_str!("../../../examples/multi_branch_die.rscel");
const BENCH_LOOP_OPAQUE: &str = include_str!("../../../bench/loop_sum_opaque.rscel");
const BENCH_PIPELINE: &str = include_str!("../../../bench/pipeline_and.rscel");
const BENCH_READ_SHARE: &str = include_str!("../../../bench/read_share.rscel");

/// `jump and` 收尾、没有 `exit` 的 Cell：靠 codegen 的兜底 return 收敛（fire-and-forget）。
const FIRE_AND_FORGET: &str = "\
entry main;

cell main;
receive();
let items = [1i64, 2i64];
jump and w over items concurrent_limit = 2 group g1;
";

/// 组里有实例 `die`：`died_in_slots` 与 `release died=` 两格的口径差在这里能跑出来。
const DIE_IN_GROUP: &str = "\
entry main;

cell main;
receive();
let items = [1i64, 2i64];
jump and w over items concurrent_limit = 1 group g1;
jump sync g1;
out(\"synced\");
exit go with();

cell w;
receive(x: i64);
die;
";

/// §6.3 `jump recurse` 内联路径（D3：紧邻 `jump sync` ⇒ 内联 await，不 spawn）。
const RECURSE_INLINE: &str = "\
entry main;

cell main;
receive();
let mut r: Option<i64> = None;
jump recurse fact go with(5i64 -> n) return(product -> r) group r0;
jump sync r0;
out(\"result = \", r.unwrap_or(-1));
exit go with();

cell fact;
receive(n: i64);
let mut sub_result: Option<i64> = None;
jump recurse fact when n > 1 go with(n - 1 -> n) return(product -> sub_result) group r1;
jump sync r1;
let result = n * sub_result.unwrap_or(1);
exit go with(result -> product);
";

/// Cell 内 `extern rust {}` + 透传 `.await`（唯一让 Cell 变 async 的原因，批次 A 的规则）。
const ASYNC_WITH_EXTERN: &str = "\
entry start;

extern rust {
    async fn rsc_slow(x: i64) -> i64 { x * 2 }
}

cell start;
receive(g1.W1.d: i64, g1.W2.d: i64, g1.W3.d: i64) as (a, b, c);
let items = [1, 2, 3];
jump and worker over items group g1;
jump sync g1;
out(\"sum=\", a + b + c);
exit go with();

cell worker;
receive(x: i64);
extern rust { let y: i64 = rsc_slow(x).await; };
exit go with(y -> d);
";

/// 测试矩阵：展开 / 独立函数 / 组派生 / 多包 receive / die / recurse / async / 顶层与块内
/// extern rust / 四个门禁场景里的三个，全都过一遍同一条断言。
fn matrix() -> Vec<(&'static str, &'static str)> {
    vec![
        ("factorial_loop（单调用点被展开 + jump loop）", FACTORIAL),
        ("jump_and_worker（组派生 + 限流 + 多包 receive + sync）", WORKER),
        ("extern_rust_mix（顶层 extern rust）", EXTERN_MIX),
        ("multi_branch_die（带 when 的 plain jump + die）", MULTI_DIE),
        ("fire_and_forget（jump and 收尾、没有 exit）", FIRE_AND_FORGET),
        ("die_in_group（组里有实例 die）", DIE_IN_GROUP),
        ("recurse_inline（D3 内联 await 路径）", RECURSE_INLINE),
        ("async_with_extern（Cell 内 extern rust + .await）", ASYNC_WITH_EXTERN),
        ("bench/loop_sum_opaque（门禁场景）", BENCH_LOOP_OPAQUE),
        ("bench/pipeline_and（门禁场景）", BENCH_PIPELINE),
        ("bench/read_share（门禁场景）", BENCH_READ_SHARE),
    ]
}

/// 解析 → 生成（可按 `trace` 开关）→ 同时把 `.map.json` 读回结构化表。
fn gen(src: &str, trace: bool) -> (Vec<String>, SrcMap) {
    let parsed = parse_program(src, 0);
    assert!(
        !parsed.has_errors(),
        "样例应可无错解析: {:#?}",
        parsed.diagnostics
    );
    let g = generate_as_with(&parsed.program, "trace_case.rs", GenOptions { trace });
    let text = g.source_map.clone().expect("必须产出 §12.9 的映射表");
    let map = parse_srcmap_json(&text).expect("自产的 .map.json 必须能读回");
    assert_eq!(text, map.to_json(), "JSON 往返必须稳定");
    (g.rust_source.lines().map(|l| l.to_string()).collect(), map)
}

/// DSL 源里第一条 trim 后等于 `needle` 的行号（1 起始）—— 断言里的行号一律这么算，
/// 不写死数字（写死等于把测试绑在样例的排版上）。
fn dsl_line(src: &str, needle: &str) -> usize {
    src.lines()
        .position(|l| l.trim() == needle)
        .unwrap_or_else(|| panic!("DSL 源里找不到 {needle:?}"))
        + 1
}

/// 取出一条生成行里 `[rsc] ` 之后的事件文本。
fn trace_msg(line: &str) -> Option<&str> {
    line.split("[rsc] ").nth(1)?.split('"').next()
}

/// 所有含 `[rsc] ` 的生成行（埋点行在生成物里都是整行的 `eprintln!`）。
fn trace_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l.contains("[rsc] "))
        .cloned()
        .collect()
}

/// 去掉尾部那个 `recv=[..]` / `send=[..]` 列表（列表里可能有 `, ` 空格，先摘掉才好按词切分）。
/// 词汇表里列表**恒在末尾**，所以只需查尾缀。
fn strip_trailing_list(msg: &str) -> Option<(String, bool)> {
    for marker in [" recv=[", " send=["] {
        if let Some(i) = msg.find(marker) {
            if !msg.ends_with(']') {
                return None;
            }
            return Some((msg[..i].to_string(), true));
        }
    }
    Some((msg.to_string(), false))
}

/// 去掉 ` @ 行:列` 并核对它确实是两个十进制数。
fn strip_pos(msg: &str) -> Option<String> {
    let toks: Vec<&str> = msg.split(' ').filter(|t| !t.is_empty()).collect();
    let mut out: Vec<&str> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == "@" {
            let pos = *toks.get(i + 1)?;
            let (l, c) = pos.split_once(':')?;
            if l.is_empty()
                || c.is_empty()
                || !l.chars().all(|ch| ch.is_ascii_digit())
                || !c.chars().all(|ch| ch.is_ascii_digit())
            {
                return None;
            }
            i += 2;
            continue;
        }
        out.push(toks[i]);
        i += 1;
    }
    Some(out.join(" "))
}

/// 按 §15.3 词汇表判一条埋点的形状，返回事件名；形状不合词汇表返回 `None`。
///
/// 为什么要这么费劲（而不是 `contains("spawn")` 之类）：这条函数等价于把词汇表
/// 写成了**可执行的规格**。将来谁改了一个字段名、多加了一格、或把位置写到了列表后面，
/// [`every_trace_line_matches_the_documented_vocabulary]` 会当场红，
/// 而不是让编辑器的解析器悄悄退化。
fn trace_event(msg: &str) -> Option<&'static str> {
    let (body, has_list) = strip_trailing_list(msg)?;
    let norm = strip_pos(&body)?;
    let t: Vec<&str> = norm.split(' ').filter(|s| !s.is_empty()).collect();
    match t.first().copied()? {
        "cell" => match t.get(2).copied()? {
            "spawn" if t.len() == 3 && has_list => Some("cell_spawn"),
            "jump" | "recurse" if t.len() == 5 && t[3] == "->" && has_list => Some("cell_send"),
            "exit" if t.len() == 3 && has_list => Some("cell_exit"),
            "implicit" if t.len() == 4 && t[3] == "exit" && has_list => Some("cell_implicit_exit"),
            "die" if t.len() == 3 && !has_list => Some("cell_die"),
            _ => None,
        },
        "group" => match t.get(2).copied()? {
            "derive"
                if t.len() == 8
                    && t[4] == "->"
                    && t[6] == "limit={}"
                    && t[7] == "items={}"
                    && has_list =>
            {
                Some("group_derive")
            }
            "permit"
                if t.len() == 5
                    && t[3].starts_with("cell=")
                    && t[4] == "acquire"
                    && !has_list =>
            {
                Some("group_permit_acquire")
            }
            "permit"
                if t.len() == 6
                    && t[3].starts_with("cell=")
                    && t[4] == "release"
                    && t[5] == "died={}"
                    && !has_list =>
            {
                Some("group_permit_release")
            }
            "done"
                if t.len() == 6
                    && t[3].starts_with("cell=")
                    && t[4] == "slots={}"
                    && t[5] == "died_in_slots={}"
                    && !has_list =>
            {
                Some("group_done")
            }
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------- 1. release 零埋点

/// `GenOptions::trace = false`（= `rscel build --release` 走的那条）时，生成物里
/// 不允许出现任何埋点痕迹 —— 埋点是 `--debug` 专属的，release 生成物是性能门禁的
/// 被测对象（`scripts/bench/run.sh`），一行都不许多。
#[test]
fn release_build_has_no_trace_artifact_in_any_shape() {
    for (name, src) in matrix() {
        let (lines, _map) = gen(src, false);
        let text = lines.join("\n");
        assert!(!text.contains("[rsc]"), "{name}: release 生成物里出现了埋点\n{text}");
        // 埋点版闭包的临时变量名（注意不能直接查 `__rsc_r`：组结果变量 `__rsc_res_g1`
        // 以它为前缀，那是 release 本来就有的代码）。
        assert!(
            !text.contains("let __rsc_r =") && !lines.iter().any(|l| l.trim() == "__rsc_r"),
            "{name}: release 里出现了埋点闭包"
        );
        assert!(!text.contains("died"), "{name}: release 里出现了 died 标记");
        // release 的令牌闭包必须是单行原样（埋点版会把它换成多行块）。
        for l in &lines {
            if l.contains("__rsc_group_run_sync(") {
                assert!(
                    l.contains("|__rsc_it| ") && l.trim_end().ends_with(");"),
                    "{name}: release 的组调用行被改写了: {l}"
                );
            }
        }
    }
}

/// debug 生成物 = release 生成物 **+ 整行新增**，唯一的结构替换是令牌闭包那一处。
///
/// 这条比「删掉 `[rsc]` 行就相等」弱一点，但它是**真的**：release 的每一行都必须按顺序
/// 出现在 debug 里；debug 独有的行只能是 ① 一条埋点 `eprintln!`，或 ② 令牌闭包包装出来的
/// 三行。谁把埋点写成内联子表达式（改动 release 已有的某一行），这里立刻红。
///
/// 闭包那一处要**成对**认账：release 的 `__rsc_group_run_sync(.., |__rsc_it| 调用);` 行在
/// debug 里变成了同一调用的 `.., |__rsc_it| {` 开头行。不把它算成"已解释"，贪心匹配会
/// 卡在这一行上，把后面所有正常的行都误报成非法新增（第一版就是这么红的）。
#[test]
fn debug_output_adds_only_whole_lines_apart_from_the_permit_closure() {
    const GROUP_CALL: &str = "__rsc_group_run_sync(";

    for (name, src) in matrix() {
        let (rel, _m1) = gen(src, false);
        let (dbg, _m2) = gen(src, true);

        let mut p = 0usize;
        let mut replaced_sites = 0usize;
        let mut illegal_extra: Vec<&String> = Vec::new();
        for d in &dbg {
            if p < rel.len() {
                if rel[p] == *d {
                    p += 1;
                    continue;
                }
                if rel[p].contains(GROUP_CALL)
                    && d.contains(GROUP_CALL)
                    && d.trim_end().ends_with("|__rsc_it| {")
                {
                    p += 1;
                    replaced_sites += 1;
                    continue;
                }
            }
            let trimmed = d.trim();
            let ok = trimmed.starts_with("eprintln!(\"[rsc] ")
                || (trimmed.starts_with("let __rsc_r = ") && trimmed.ends_with(';'))
                || trimmed == "__rsc_r"
                || trimmed == "});";
            if !ok {
                illegal_extra.push(d);
            }
        }
        assert_eq!(
            p,
            rel.len(),
            "{name}: release 有 {} 行没在 debug 里按顺序出现（埋点删了代码？）",
            rel.len() - p
        );
        assert_eq!(
            replaced_sites,
            rel.iter().filter(|l| l.contains(GROUP_CALL)).count(),
            "{name}: 组调用行的替换处数对不上"
        );
        assert!(
            illegal_extra.is_empty(),
            "{name}: debug 里出现了不合法的独有行: {illegal_extra:?}"
        );
    }
}

/// 每一条埋点都必须长得像词汇表里那一格（把词汇表变成可执行的规格）。
#[test]
fn every_trace_line_matches_the_documented_vocabulary() {
    for (name, src) in matrix() {
        let (dbg, _map) = gen(src, true);
        let lines = trace_lines(&dbg);
        assert!(!lines.is_empty(), "{name}: 开了埋点却一行埋点都没有");
        for l in &lines {
            let msg = trace_msg(l).unwrap_or_else(|| panic!("{name}: 埋点行形状不对: {l}"));
            assert!(
                trace_event(msg).is_some(),
                "{name}: 埋点文本不在 §15.3 词汇表里: {l}"
            );
        }
    }
}

// ---------------------------------------------------------------- 2. 内容对得上 DSL

/// `jump_and_worker` 是整个词汇表最密的一份样例：九种事件里它占七种，
/// 而且每条都带 DSL 位置或键名 —— 编辑器要的正是这两样。
#[test]
fn worker_group_trace_lines_carry_dsl_positions_and_key_names() {
    let (dbg, _map) = gen(WORKER, true);
    let text = dbg.join("\n");

    let expect = |needle: String, why: &str| {
        assert!(text.contains(&needle), "缺少 {why}，期望出现: {needle}\n实际埋点:\n{}", trace_lines(&dbg).join("\n"));
    };

    // 实例创建 + 收到 Packet：多包键按 DSL 写法（带包前缀），不是内部 v_<id>。
    expect(
        format!(
            "[rsc] cell start spawn @ {}:1 recv=[g1.W1.d, g1.W2.d, g1.W3.d]",
            dsl_line(WORKER, "cell start;")
        ),
        "父 Cell 的 spawn + 多包 recv 键名",
    );
    // 整组派生规模：limit/items 是运行期值，所以留 `{}` 给 eprintln! 的实参。
    expect(
        format!(
            "[rsc] group g1 derive start -> worker limit={{}} items={{}} @ {}:1 send=[]",
            dsl_line(WORKER, "jump and worker over items concurrent_limit=2 group g1;")
        ),
        "jump and 的整组派生行",
    );
    expect(
        "[rsc] group g1 permit cell=worker acquire".to_string(),
        "并发令牌申请",
    );
    expect(
        "[rsc] group g1 permit cell=worker release died={}".to_string(),
        "并发令牌归还 + 该实例是否 die",
    );
    expect(
        format!(
            "[rsc] group g1 done cell=worker slots={{}} died_in_slots={{}} @ {}:1",
            dsl_line(WORKER, "jump sync g1;")
        ),
        "jump sync 处的组完成状态（位置必须是 sync 那一行，不是创建组那一行）",
    );
    expect(
        format!(
            "[rsc] cell start exit @ {}:1 send=[]",
            dsl_line(WORKER, "exit go with();")
        ),
        "父 Cell 的 exit",
    );
    expect(
        format!(
            "[rsc] cell worker spawn @ {}:1 recv=[x]",
            dsl_line(WORKER, "cell worker;")
        ),
        "worker 实例的 spawn",
    );
    expect(
        format!(
            "[rsc] cell worker exit @ {}:1 send=[d]",
            dsl_line(WORKER, "exit go with(x * 2 -> d);")
        ),
        "worker 的 exit + 交回的键名（键名来自 `-> d`）",
    );

    // 这份样例没有 jump loop / plain jump，埋点总数因此是确定的 8 条：
    // 多一条就是有人在不该插的地方插了。
    assert_eq!(
        trace_lines(&dbg).len(),
        8,
        "jump_and_worker 的埋点条数应与词汇表的 8 条一一对应"
    );
}

/// 被**实体展开**的 Cell（候选 1，P0-4）没有独立函数了，spawn/exit 一行都不能少 ——
/// 展开恰恰是热 Cell 的默认形状，漏掉就是追踪流里凭空少一个实例。
#[test]
fn expanded_cell_still_reports_spawn_and_exit() {
    let (dbg, _map) = gen(FACTORIAL, true);
    let text = dbg.join("\n");
    assert!(
        !text.contains("fn cell_fact("),
        "前提：factorial 的 `fact` 已被展开成块表达式（没有独立函数）"
    );
    assert!(
        text.contains(&format!(
            "[rsc] cell fact spawn @ {}:1 recv=[n, acc]",
            dsl_line(FACTORIAL, "cell fact;")
        )),
        "展开体里必须有被调 Cell 的 spawn 埋点"
    );
    assert!(
        text.contains(&format!(
            "[rsc] cell fact exit @ {}:1 send=[result]",
            dsl_line(FACTORIAL, "exit go with(next_acc -> result);")
        )),
        "展开体里必须有被调 Cell 的 exit 埋点（它的键名）"
    );
    // 发送侧：`jump fact go with(five -> n, 1i64 -> acc)` 的键名是 `-> k` 的那些。
    assert!(
        text.contains(&format!(
            "[rsc] cell main jump -> fact @ {}:1 send=[n, acc]",
            dsl_line(FACTORIAL, "jump fact go with(five -> n, 1i64 -> acc);")
        )),
        "plain jump 的发送侧必须列出这次 Packet 的键名"
    );
}

/// `jump loop` **有意不插**（§9.2：它按位置原地刷新 `v_<id>`，不搬 Packet；且每轮一次）。
///
/// 断言用条数而不是「文本里没有 loop 字样」：`fact` 的 loop 体要跑 5 轮，
/// 一旦有人给 jump loop 加埋点，条数就会从 4 变成 4+5×k，这条立刻红。
#[test]
fn jump_loop_is_not_traced() {
    let (dbg, _map) = gen(FACTORIAL, true);
    let lines = trace_lines(&dbg);
    assert_eq!(
        lines.len(),
        4,
        "factorial_loop 只有 main 的 spawn/jump + 展开体 fact 的 spawn/exit 四条；实际:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines.iter().any(|l| l.contains("loop")),
        "jump loop 不该产生埋点行: {lines:?}"
    );
}

/// fire-and-forget（body 以 `jump and` 收尾、没有 `exit`）走的是 codegen 的兜底 return。
/// 那条路径**可运行到**（check 认 `jump and` 为终结语句），不补一行就是
/// 「这个实例凭空消失」。
#[test]
fn fire_and_forget_cell_reports_its_implicit_exit() {
    let (rel, _m1) = gen(FIRE_AND_FORGET, false);
    let (dbg, _map) = gen(FIRE_AND_FORGET, true);
    let text = dbg.join("\n");
    assert!(
        text.contains(&format!(
            "[rsc] cell main implicit exit @ {}:1 send=[]",
            dsl_line(FIRE_AND_FORGET, "cell main;")
        )),
        "兜底 return 之前必须有一行隐式收敛埋点\n实际:\n{}",
        trace_lines(&dbg).join("\n")
    );
    assert!(
        !rel.join("\n").contains("implicit exit"),
        "release 里不该有任何埋点"
    );
    // 兜底 return 与它前面的埋点都不对应任何 DSL 语句 → 不进 §12.9 映射（既有约定）。
    // 只查这一行的 lookup：文件里 `fn main` 的条目排在 Cell 之后，所以不能拿
    // 「最后一条目的结束行」来比。
    let at = dbg
        .iter()
        .position(|l| l.contains("implicit exit"))
        .map(|i| i + 1)
        .expect("implicit exit 行");
    assert!(
        _map.lookup(at).is_none(),
        "隐式收敛埋点必须落在未映射区（与它下面那行兜底 return 同一待遇）"
    );
}

// ---------------------------------------------------------------- 3. 不破坏既有边界

/// §15.3 原文「`extern rust` 块内不插」。
///
/// 判据用映射表的 `in_extern` 区间，而不是在样例文本里找花括号：区间是 CLI 做
/// RSC-E901 判定时用的同一份数据，所以这条断言与反映射行为**同源**。
#[test]
fn trace_lines_never_appear_inside_extern_rust_ranges() {
    let mut saw_extern_range = false;
    for (name, src) in matrix() {
        let (dbg, map) = gen(src, true);
        for e in map.entries.iter().filter(|e| e.in_extern) {
            saw_extern_range = true;
            for ln in e.rs_line_start..=e.rs_line_end {
                assert!(
                    !dbg[ln - 1].contains("[rsc] "),
                    "{name}: extern rust 区间 {}..={} 的第 {ln} 行被插了埋点: {:?}",
                    e.rs_line_start,
                    e.rs_line_end,
                    dbg[ln - 1]
                );
            }
        }
    }
    assert!(
        saw_extern_range,
        "矩阵里至少要有 `extern rust{{}}` 的区间，否则这条测试什么都没测"
    );
}

/// 埋点主要在**已有区间内**多出整行：release 的条目序列必须是 debug 条目序列的
/// 子序列（同序、同类型、同 DSL 位置、区间只会长）。
///
/// 但有一个实测出来的例外，写在注释里而不是藏起来：`SrcMapBuilder::push` 对**零行区间**
/// 不记条目，而 `cell x;` + 空 `receive()` 的绑定块正是零行；埋点在里面加了一行 spawn，
/// 于是 debug 会**多出**一条 `receive(...)` 的条目。这条测试因此要求：多出来的条目
/// 只能整段由埋点行组成（它映射的就是埋点自己，不会把任何既有代码从映射里挪走）。
#[test]
fn debug_srcmap_keeps_the_sorted_non_overlapping_invariant() {
    for (name, src) in matrix() {
        let (_rel_lines, rel) = gen(src, false);
        let (dbg, dbg_map) = gen(src, true);

        for map in [&rel, &dbg_map] {
            let mut prev_end = 0usize;
            for e in &map.entries {
                assert!(e.rs_line_start <= e.rs_line_end, "{name}: 区间倒挂 {e:?}");
                assert!(
                    e.rs_line_start > prev_end,
                    "{name}: 条目必须升序且互不重叠，{}..={} 与上一条的 {prev_end} 冲突",
                    e.rs_line_start,
                    e.rs_line_end
                );
                prev_end = e.rs_line_end;
            }
        }

        let mut ri = 0usize;
        let mut extra_dbg_entries = 0usize;
        for de in &dbg_map.entries {
            if ri < rel.entries.len() {
                let re = &rel.entries[ri];
                if re.kind == de.kind && re.dsl == de.dsl {
                    assert!(
                        de.rs_line_end - de.rs_line_start >= re.rs_line_end - re.rs_line_start,
                        "{name}: 埋点让区间变短了（除非它删了代码行）: {re:?} -> {de:?}"
                    );
                    ri += 1;
                    continue;
                }
            }
            // debug 独有的条目：整段必须都是埋点行。
            for ln in de.rs_line_start..=de.rs_line_end {
                assert!(
                    dbg[ln - 1].contains("[rsc] "),
                    "{name}: 新增条目 {de:?} 的第 {ln} 行不是埋点: {:?}",
                    dbg[ln - 1]
                );
            }
            extra_dbg_entries += 1;
        }
        assert_eq!(ri, rel.entries.len(), "{name}: release 的条目没被逐一对上");
        let _ = extra_dbg_entries;

        // 除「隐式收敛」那一格（它本来就在未映射区，与兜底 return 同一待遇），
        // 每条埋点都必须被某条映射接管 —— 否则 rustc 在这行报错时反映射会掉进黑洞。
        for (i, l) in dbg.iter().enumerate() {
            if !l.contains("[rsc] ") || l.contains("implicit exit") {
                continue;
            }
            let ln = i + 1;
            assert!(
                dbg_map.lookup(ln).is_some(),
                "{name}: 第 {ln} 行埋点没有被任何映射接管: {l}"
            );
        }
    }
}

// ---------------------------------------------------------------- 4. 真编译真运行

/// 把生成物交给真 rustc 编译并运行（与 `gen_smoke` 同一手法），返回 (stdout, stderr)。
fn compile_and_run(lines: &[String], tag: &str) -> (String, String) {
    let dir = std::env::temp_dir().join(format!("rscel-gen-trace-{tag}"));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let rs = dir.join(format!("{tag}.rs"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&rs, lines.join("\n") + "\n").expect("写生成物");
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg("-O")
        .arg(&rs)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("调用 rustc");
    assert!(status.success(), "{tag}: rustc 编译失败（生成物 {} 行）", lines.len());
    let out = Command::new(&exe).output().expect("运行产物");
    assert!(out.status.success(), "{tag}: 运行失败: {out:?}");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 端到端：`--debug` 的生成物编得过、跑得动，事件**按运行期的真实事实**出现。
///
/// 这里验的是静态文本永远验不到的三件事：
/// - 埋点走 **stderr**，stdout 一行都没被污染（编辑器的「构建 + 运行」还要拿 stdout 的结果行）；
/// - 每实例一对 `acquire/release`，`died=true` 真的来自 `die`；
/// - 事件的先后顺序符合语义（spawn 在派生之前、done 在 sync 之后、父 exit 在最后）。
#[test]
fn debug_generated_program_compiles_and_streams_events() {
    let (dbg, _map) = gen(DIE_IN_GROUP, true);
    let (stdout, stderr) = compile_and_run(&dbg, "die-in-group");

    assert!(
        !stdout.contains("[rsc]"),
        "埋点必须走 stderr，stdout 不能被污染: {stdout:?}"
    );
    assert!(stdout.contains("synced"), "用户程序自己的输出还在: {stdout:?}");

    let count = |needle: String| stderr.matches(needle.as_str()).count();
    assert_eq!(
        count(format!(
            "[rsc] cell w spawn @ {}:1 recv=[x]",
            dsl_line(DIE_IN_GROUP, "cell w;")
        )),
        2,
        "两个派生实例，两条 spawn。stderr:\n{stderr}"
    );
    assert_eq!(count("[rsc] group g1 permit cell=w acquire".to_string()), 2);
    assert_eq!(
        count("[rsc] group g1 permit cell=w release died=true".to_string()),
        2,
        "两个实例都 die 了，died 必须是 true"
    );
    assert_eq!(
        count(format!("[rsc] cell w die @ {}:1", dsl_line(DIE_IN_GROUP, "die;"))),
        2,
        "die 行号取自 DSL"
    );
    assert_eq!(
        count(format!(
            "[rsc] group g1 derive main -> w limit=1 items=2 @ {}:1 send=[]",
            dsl_line(DIE_IN_GROUP, "jump and w over items concurrent_limit = 1 group g1;")
        )),
        1,
        "limit/items 两格必须是运行期的真实值（1 与 2）"
    );
    assert_eq!(
        count(format!(
            "[rsc] group g1 done cell=w slots=0 died_in_slots=0 @ {}:1",
            dsl_line(DIE_IN_GROUP, "jump sync g1;")
        )),
        1
    );

    // 顺序：spawn(父) → derive → （实例事件）→ done → 父 exit。
    let at = |needle: &str| stderr.find(needle).unwrap_or_else(|| panic!("缺少 {needle}"));
    assert!(at("[rsc] cell main spawn") < at("[rsc] group g1 derive"));
    assert!(at("[rsc] group g1 derive") < at("[rsc] group g1 done"));
    assert!(at("[rsc] group g1 done") < at("[rsc] cell main exit"));
}

/// 同一个样例关掉埋点：输出必须与埋点版**只差 stderr 的那些行**，stdout 逐字相同。
///
/// 这条是「埋点不改语义」的最小证据（另一条是上面 release 零埋点 + 逐行等价）。
#[test]
fn release_generated_program_streams_nothing_and_prints_the_same() {
    let (rel, _m1) = gen(DIE_IN_GROUP, false);
    let (dbg, _m2) = gen(DIE_IN_GROUP, true);
    let (rel_out, rel_err) = compile_and_run(&rel, "die-in-group-release");
    let (dbg_out, dbg_err) = compile_and_run(&dbg, "die-in-group-debug");

    assert_eq!(rel_out, dbg_out, "stdout 必须逐字相同");
    assert!(rel_err.is_empty(), "release 产物不该往 stderr 写任何东西: {rel_err:?}");
    assert!(dbg_err.contains("[rsc] "), "debug 产物应有埋点: {dbg_err:?}");
}
