//! rustc 错误反映射（§15.2）。
//!
//! 流水线位置：`rscel build` → `rustc --error-format=json` 捕获 stderr → 本模块把
//! 每条诊断的 `spans[].line_start` 拿去查 §12.9 的 `.map.json` → 渲染成 DSL 作者看得懂的
//! `RSC-E900` / `RSC-E901` / 未映射说明，写 stderr。
//!
//! ## 为什么用 `--error-format=json` 而不是 `--message-format=json`
//!
//! 交接文档 §7 原先记的是「rustc 不支持 `--message-format` flag」——**结论对，归因错**：
//! `--message-format` 是 **cargo** 的 flag，rustc 对应的是 `--error-format`。
//! 实测（rustc 1.97.1，本机）：
//! ```text
//! rustc --message-format=json a.rs  → error: Unrecognized option: 'message-format'（退出 1）
//! rustc --error-format=json   a.rs  → stderr 变成每行一个 JSON 对象（NDJSON）
//! ```
//! 所以规格 §15.2 写的 `--message-format=json` 在 rustc 上落地为 `--error-format=json`。
//!
//! ## 为什么输出渲染是纯函数
//!
//! 项目规约要求测试能断言诊断文本。`eprintln!` 打到真实 stderr，集成测试拿不到；
//! [`render_diagnostics`] 返回 `String`，CLI 层只负责打印它，测试直接喂真实 rustc
//! 产出的 NDJSON（或手工构造的样本）断言文本 —— 也顺带保证「退出码 2」这条路径
//! 不需要依赖 workspace 里不稳定的二进制路径。

use rscel_gen::srcmap::{parse_json, Json, MapEntry, SrcMap};

/// §15.2：rustc 错误落在 DSL 语句区间内（非 extern）时的错误码。
pub const CODE_MAPPED: &str = "RSC-E900";
/// §15.2：rustc 错误落在 `extern rust {}` 区间内（不做反映射）时的错误码。
pub const CODE_IN_EXTERN: &str = "RSC-E901";

/// 一条 rustc 诊断（只取反映射需要的字段，其余字段丢弃是**有意**的：
/// `children` 里的建议片段、`note` 之类不影响定位，全量转发会把用户淹死）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustcDiag {
    /// `message`（rustc 的一句话诊断）。
    pub message: String,
    /// `code.code`，如 `E0308`；lint 类诊断没有 code。
    pub code: Option<String>,
    /// `level`：`error` / `warning` / `note` / `failure-note` / `help`。
    pub level: String,
    /// 主 span 的文件名；无 span 时为空。
    pub file: String,
    /// 主 span 的起始行（1 起始）；无 span 时为 0。
    pub line: usize,
    /// 主 span 的起始列（1 起始，按字符）；无 span 时为 0。
    pub col: usize,
    /// 是否带可用位置（`line == 0` 的汇总信息如 "aborting due to ..." 为 false）。
    pub has_span: bool,
}

impl RustcDiag {
    /// `error[E0308]: mismatched types` 形式的一行摘要。
    pub fn summary(&self) -> String {
        let level = self.level_word();
        match &self.code {
            Some(c) => format!("{level}[{c}]: {}", self.message),
            None => format!("{level}: {}", self.message),
        }
    }

    fn level_word(&self) -> &str {
        match self.level.as_str() {
            "error" => "error",
            "warning" => "warning",
            "note" => "note",
            "failure-note" => "note",
            "help" => "help",
            other => other,
        }
    }

    /// 是否 error 级（决定 `build` 的成败；CLI 只用 rustc 退出码判成败，
    /// 这个判定用于「成功但带 warning」时过滤掉 note/help 之类噪声的补充说明）。
    pub fn is_warning(&self) -> bool {
        self.level == "warning"
    }
}

/// 解析 NDJSON 的一行。
///
/// 返回 `None` 表示「这行不是 rustc 的诊断 JSON」（空行、或 rustc 启动失败时的
/// 纯文本 `error: Unrecognized option: ...`）—— 调用方必须把它**原样输出**，
/// 绝不静默丢弃（§15.2 第 4 条：多个诊断全部输出）。
pub fn parse_diagnostic(line: &str) -> Option<RustcDiag> {
    let t = line.trim();
    if t.is_empty() || !t.starts_with('{') {
        return None;
    }
    let root = parse_json(t).ok()?;
    // rustc 的 NDJSON 每行都带 `"$message_type": "diagnostic"`。
    // **不**用它做过滤门槛：未来 rustc 若新增其它类型（或旧版本没这个字段），
    // 拿它当筛子就等于把真诊断丢了（§15.2 第 4 条要求全部输出）。
    let message = root.get("message").and_then(Json::as_str).unwrap_or("").to_string();
    let code = root
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Json::as_str)
        .map(|s| s.to_string());
    let level = root.get("level").and_then(Json::as_str).unwrap_or("error").to_string();

    // 主 span 优先（`is_primary: true`），没有 primary 就取第一个 span —— §15.2 第 2 条。
    let mut chosen: Option<(String, usize, usize)> = None;
    if let Some(Json::Arr(spans)) = root.get("spans") {
        for sp in spans {
            let file = sp.get("file_name").and_then(Json::as_str).unwrap_or("").to_string();
            let line_no = sp.get("line_start").and_then(Json::as_usize).unwrap_or(0);
            let col = sp.get("column_start").and_then(Json::as_usize).unwrap_or(0);
            let primary = sp.get("is_primary").and_then(Json::as_bool).unwrap_or(false);
            if primary {
                chosen = Some((file, line_no, col));
                break;
            }
            if chosen.is_none() && line_no != 0 {
                chosen = Some((file, line_no, col));
            }
        }
    }
    let (file, line_no, col) = chosen.unwrap_or_default();
    let has_span = line_no != 0;
    Some(RustcDiag { message, code, level, file, line: line_no, col, has_span })
}

/// 用映射表把诊断定位换回 DSL 位置。
///
/// 三个前提缺一不可，否则视为「查不到映射」：
/// 1. 诊断带行号（汇总类信息没有 span）；
/// 2. `file_name` 以映射表的 `file` 结尾 —— 防止 `include!` / 宏展开带来的**别的文件**
///    的行号被误当成我们生成文件的行号（那种行号一定在表里，会指到完全无关的语句）；
/// 3. 行号命中某条区间，且该区间有可用的 DSL 位置（`line != 0`）。
pub fn lookup_entry<'a>(diag: &RustcDiag, map: &'a SrcMap) -> Option<&'a MapEntry> {
    if !diag.has_span {
        return None;
    }
    if map.file.is_empty() || !diag.file.ends_with(map.file.as_str()) {
        return None;
    }
    let entry = map.lookup(diag.line)?;
    if entry.dsl.has_position() {
        Some(entry)
    } else {
        None
    }
}

/// 渲染一条诊断的 stderr 文本（三种归宿之一，见 §15.2 第 3 条）。
///
/// `dsl_file` 是 `.rscel` 源文件路径，只用在映射成功的行上，方便用户直接跳编辑器。
pub fn render_one(diag: &RustcDiag, map: &SrcMap, dsl_file: &str) -> String {
    let generated_pos = if diag.has_span {
        format!("{}:{}:{}", diag.file, diag.line, diag.col)
    } else {
        "（无源码位置）".to_string()
    };
    match lookup_entry(diag, map) {
        Some(entry) if entry.in_extern => format!(
            "{CODE_IN_EXTERN}: extern rust 块内的 Rust 错误（不做反映射）: {}；rustc 位置: {generated_pos}",
            diag.summary()
        ),
        Some(entry) => {
            // error / warning 的措辞要分开：把 warning 说成「编译错误」是撒谎，
            // 而 §15.2 只给了 E900 一个码，所以沿用 E900、只替换名词。
            let noun = if diag.level == "error" { "rustc 编译错误" } else { "rustc 警告" };
            format!(
                "{CODE_MAPPED}: {noun}（映射自 DSL 位置 cell {} @ {}:{}）: {}；生成位置: {generated_pos}；DSL 源: {dsl_file}:{}:{}",
                entry.dsl.cell, entry.dsl.line, entry.dsl.col, diag.summary(), entry.dsl.line, entry.dsl.col
            )
        }
        None if diag.has_span => format!(
            "rustc 诊断（位于编译器自身生成区间：内嵌运行时 / 生成器补的控制流，无 DSL 对应位置，故不做反映射）: {}；位置: {generated_pos}",
            diag.summary()
        ),
        // 没有 span 的是 rustc 的记账行（"aborting due to N previous errors" /
        // "try --explain E0308"）。它们不是独立错误，但也不能丢——丢了一条
        // "aborting due to" 用户就不知道总错误数，丢了指令就不知道去哪儿查文档。
        None => format!("rustc（编译器汇总信息，无源码位置）: {}", diag.summary()),
    }
}

/// 把整段 rustc NDJSON  stderr 渲染成给人看的诊断文本。
///
/// 逐行处理：空行跳过；解析不出 JSON 的行**原样带上**（见 [`parse_diagnostic`] 的说明）。
///
/// `only_warnings` 用于「rustc 退出 0 但 JSON 里有 warning」的场景（§16.1：
/// 只有非零退出才算编译失败）：此时只把 warning 打到 stderr，error/note 一律不打印
/// —— 退出 0 时不会真有 error，所以这里丢的不是诊断。
pub fn render_diagnostics(ndjson: &str, map: &SrcMap, dsl_file: &str, only_warnings: bool) -> String {
    let mut out = String::new();
    for line in ndjson.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_diagnostic(line) {
            Some(diag) => {
                if only_warnings && !diag.is_warning() {
                    continue;
                }
                out.push_str(&render_one(&diag, map, dsl_file));
                out.push('\n');
            }
            // 非 JSON 行：rustc 自身的启动期错误（如 flag 不认）走这里。
            // 保留原文，否则用户只会看到「编译失败」而没有任何线索。
            None => {
                if only_warnings {
                    continue;
                }
                out.push_str("rustc: ");
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
    }
    out
}

/// 数出**带源码位置**的 error 级诊断条数（给 stderr 的汇总行用）。
///
/// 为什么排除无 span 的那几条：`error: aborting due to 1 previous error` 本身也是
/// `level == "error"`，把它计入会得到「2 条错误」这种自相矛盾的数字
/// （只有 1 条真错误 + 1 条 rustc 的记账行）。
pub fn count_errors(ndjson: &str) -> usize {
    ndjson
        .lines()
        .filter_map(parse_diagnostic)
        .filter(|d| d.level == "error" && d.has_span)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscel_gen::srcmap::{parse_srcmap_json, SrcMap};

    fn map() -> SrcMap {
        parse_srcmap_json(
            r#"{ "version": 1, "file": "app.rs", "runtime_lines": 300,
                 "cells": { "fact": { "line": 12, "col": 1 } },
                 "map": [
                   { "rs_line_start": 320, "rs_line_end": 326, "rs_col": 1, "kind": "stmt",
                     "in_extern": false, "dsl": { "cell": "fact", "stmt": 3, "line": 15, "col": 5 } },
                   { "rs_line_start": 327, "rs_line_end": 330, "rs_col": 1, "kind": "stmt",
                     "in_extern": true, "dsl": { "cell": "fact", "stmt": 4, "line": 16, "col": 1 } }
                 ] }"#,
        )
        .expect("样本 map 必须能读回")
    }

    const DIAG_MAPPED: &str = r#"{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":null},"level":"error","spans":[{"file_name":"app.rs","byte_start":1,"byte_end":4,"line_start":322,"line_end":322,"column_start":9,"column_end":12,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"..."}"#;
    const DIAG_EXTERN: &str = r#"{"$message_type":"diagnostic","message":"expected one `|`","code":null,"level":"error","spans":[{"file_name":"app.rs","byte_start":1,"byte_end":4,"line_start":328,"line_end":328,"column_start":3,"column_end":6,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"..."}"#;
    const DIAG_RUNTIME: &str = r#"{"$message_type":"diagnostic","message":"no two closures, even if identical, have the same type","code":null,"level":"error","spans":[{"file_name":"app.rs","byte_start":1,"byte_end":4,"line_start":42,"line_end":42,"column_start":1,"column_end":2,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"..."}"#;

    #[test]
    fn mapped_diagnostic_yields_e900_with_dsl_position() {
        let text = render_diagnostics(DIAG_MAPPED, &map(), "app.rscel", false);
        assert!(text.contains("RSC-E900"), "{text}");
        assert!(text.contains("cell fact @ 15:5"), "{text}");
        assert!(text.contains("E0308"), "{text}");
        assert!(text.contains("app.rs:322:9"), "{text}");
    }

    #[test]
    fn extern_range_yields_e901_without_dsl_line() {
        let text = render_diagnostics(DIAG_EXTERN, &map(), "app.rscel", false);
        assert!(text.contains("RSC-E901"), "{text}");
        assert!(!text.contains("@ 16:1"), "extern 块不该做 DSL 行映射: {text}");
    }

    #[test]
    fn unmapped_range_is_not_dropped() {
        let text = render_diagnostics(DIAG_RUNTIME, &map(), "app.rscel", false);
        assert!(text.contains("编译器自身生成区间"), "{text}");
        assert!(text.contains("app.rs:42:1"), "{text}");
    }

    #[test]
    fn foreign_file_line_is_not_mapped() {
        // 行号在表里，但文件名不是我们的生成文件（include!/宏展开）→ 绝不反映射。
        let line = DIAG_MAPPED.replace("\"app.rs\"", "\"other.rs\"");
        let text = render_diagnostics(&line, &map(), "app.rscel", false);
        assert!(text.contains("编译器自身生成区间") && !text.contains("RSC-E900"), "{text}");
    }

    #[test]
    fn non_json_line_survives() {
        let text = render_diagnostics("error: Unrecognized option: 'message-format'", &map(), "app.rscel", false);
        assert!(text.contains("Unrecognized option"), "{text}");
    }

    #[test]
    fn only_warnings_filters_out_notes() {
        let note = r#"{"$message_type":"diagnostic","message":"aborting due to 1 previous error","code":null,"level":"error","spans":[],"children":[],"rendered":null}"#;
        let warn = r#"{"$message_type":"diagnostic","message":"unused variable: `x`","code":null,"level":"warning","spans":[{"file_name":"app.rs","line_start":322,"column_start":9,"is_primary":true}],"children":[],"rendered":null}"#;
        let text = render_diagnostics(&format!("{note}\n{warn}"), &map(), "app.rscel", true);
        assert!(!text.contains("aborting"), "{text}");
        assert!(text.contains("unused variable"), "{text}");
        // 汇总行（无 span）不算一条真错误，否则「1 条错误 + aborting」会被数成 2 条。
        assert_eq!(count_errors(&format!("{note}\n{warn}")), 0);
        assert_eq!(count_errors(DIAG_MAPPED), 1);
    }

    #[test]
    fn no_primary_span_falls_back_to_first_span() {
        let s = r#"{"$message_type":"diagnostic","message":"expected","level":"error","spans":[{"file_name":"app.rs","line_start":327,"column_start":2,"is_primary":false}]}"#;
        let d = parse_diagnostic(s).expect("应解析成功");
        assert_eq!((d.line, d.col), (327, 2));
    }
}
