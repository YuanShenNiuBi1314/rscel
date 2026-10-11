//! 拓扑模型测试（§16.4 调用链路报告 + 编辑器数据源）。

use rscel_check::topology::{self, EdgeKind};
use rscel_parser::parse_program;

const JUMP_AND: &str = include_str!("../../rscel-gen/examples/jump_and_worker.rscel");

#[test]
fn topology_covers_nodes_edges_and_groups() {
    let out = parse_program(JUMP_AND, 0);
    assert!(!out.has_errors(), "样例解析失败: {:#?}", out.diagnostics);
    let t = topology::topology_of(&out.program);

    assert_eq!(t.entry.as_deref(), Some("start"), "entry 应被识别");
    let names: Vec<&str> = t.nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"start") && names.contains(&"worker"), "{names:?}");

    let start = t.nodes.iter().find(|n| n.name == "start").unwrap();
    assert!(start.is_entry);
    assert!(start.reachable, "入口必然可达");
    // 多包 receive 的三个槽位都要出现在节点上（编辑器靠它画 ⇩ 列表）
    assert_eq!(start.receive.len(), 3, "{:?}", start.receive);
    assert!(start.receive.iter().all(|s| s.pkg.is_some()), "g1.Wn.* 必须保留包前缀");

    let worker = t.nodes.iter().find(|n| n.name == "worker").unwrap();
    assert_eq!(worker.outputs.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), vec!["d"]);

    // 边：一条 and（start→worker）+ 一条 sync（start→⏚g1）
    let and = t
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::And)
        .expect("应有 jump and 边");
    assert_eq!((and.from.as_str(), and.to.as_str()), ("start", "worker"));
    assert_eq!(and.group.as_deref(), Some("g1"));
    assert_eq!(and.limit.as_deref(), Some("2"), "concurrent_limit 必须进图");
    assert_eq!(and.over.as_deref(), Some("items"), "over 集合必须进图");
    let sync = t
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Sync)
        .expect("应有 jump sync 等待边");
    assert_eq!(sync.to, "⏚g1");

    // group 汇总：3 个槽位 + 有 sync
    let g = t.groups.iter().find(|g| g.name == "g1").expect("group g1");
    assert_eq!(g.slots, 3, "D5 静态槽位数");
    assert!(g.synced);
    assert_eq!(g.targets, vec!["worker".to_string()]);
}

#[test]
fn mermaid_export_is_renderable_text() {
    let out = parse_program(JUMP_AND, 0);
    let t = topology::topology_of(&out.program);
    let m = topology::to_mermaid(&t);
    assert!(m.starts_with("graph TD\n"), "必须是 mermaid graph TD：{m}");
    assert!(m.contains("\"worker\""), "{m}");
    // 边标签里带上形态、传输模式、键名、group 与 limit（§16.4 要求标注这些）
    assert!(m.contains("and"), "{m}");
    assert!(m.contains("lim=2"), "并发上限要标在边上：{m}");
    assert!(m.contains("group g1"), "{m}");
    // 管道字符在 mermaid 边标签里是分隔符，必须被替换掉
    assert!(!m.contains("|>"), "over 管道文本里的 `|` 会破坏 mermaid 边标签：{m}");
}

#[test]
fn text_report_walks_from_entry_and_lists_groups() {
    let out = parse_program(JUMP_AND, 0);
    let t = topology::topology_of(&out.program);
    let r = topology::to_text_report(&t);
    assert!(r.contains("入口: start"), "{r}");
    assert!(r.contains("并发组:"), "{r}");
    assert!(r.contains("g1 ← start"), "{r}");
    assert!(r.contains("limit=2"), "{r}");
    assert!(r.contains("父侧槽位 3 个"), "{r}");
}

/// §6.4 多目标：一条 and 边**每个目标一条**（修前 gen 只派生第一个目标，
/// 图上也只画一条边，读图的人看不出少了一路）。
#[test]
fn multi_target_and_creates_one_edge_per_target() {
    let src = r#"
cell main;
receive(g1.W1.A: i64, g1.W2.A: i64) as (a, b);
jump and p, q go with(0 -> x) group g1;
jump sync g1;
out(a, b);
exit go with();

cell p;
receive(x: i64);
exit go with(x -> A);

cell q;
receive(x: i64);
exit go with(x -> A);
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "{:#?}", out.diagnostics);
    let t = topology::topology_of(&out.program);
    let targets: Vec<&str> = t
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::And)
        .map(|e| e.to.as_str())
        .collect();
    assert_eq!(targets, vec!["p", "q"], "多目标必须各出一条边：{targets:?}");
    let g = t.groups.iter().find(|g| g.name == "g1").unwrap();
    assert_eq!(g.targets, vec!["p".to_string(), "q".to_string()]);
}

/// fire-and-forget（`jump and` 不命名 group）不是错误，但必须在图上标注；
/// 不可达 Cell 也不能被报告"假装"成链路的一部分。
#[test]
fn fire_and_forget_and_unreachable_are_marked() {
    let src = r#"
cell main;
receive();
jump and w over [1, 2];
exit go with();

cell w;
receive(x: i64);
exit go with();

cell orphan;
receive();
die;
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "{:#?}", out.diagnostics);
    let t = topology::topology_of(&out.program);
    let main = t.nodes.iter().find(|n| n.name == "main").unwrap();
    assert!(
        main.risks.iter().any(|r| r.contains("fire-and-forget")),
        "无 group 的 jump and 必须标注：{:?}",
        main.risks
    );
    assert_eq!(t.groups.len(), 0, "没有 group 名就不该有组记录");
    let orphan = t.nodes.iter().find(|n| n.name == "orphan").unwrap();
    assert!(!orphan.reachable, "orphan 不可达");
    let report = topology::to_text_report(&t);
    assert!(
        report.contains("未从入口抵达的 Cell") && report.contains("orphan（"),
        "不可达 Cell 必须单列，而不是混进链路里：{report}"
    );
}
