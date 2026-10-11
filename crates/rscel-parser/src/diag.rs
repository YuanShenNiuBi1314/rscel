//! 诊断消息（规格 §7.4 / §15.1 / 附录 B）。
//!
//! 诊断码以**数字**存放（30 / 34 / 1 …），渲染时格式化为 `RSC-E030` / `RSC-W001`。
//! 渲染格式固定为 §15.1：`RSC-<E|W><NNN>: <消息>；<细节>；位置: cell <name> @ <行>:<列>`。

use crate::span::Span;

/// 诊断级别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    /// `Error -> 'E'`，`Warning -> 'W'`。
    pub const fn tag(self) -> char {
        match self {
            Severity::Error => 'E',
            Severity::Warning => 'W',
        }
    }
}

/// 一条诊断。
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// 数字错误码，如 `30` 表示 `RSC-E030`，`1` 表示 `RSC-W001`。
    pub code: u16,
    pub severity: Severity,
    pub message: String,
    /// 补充细节（渲染时以 `；` 连接）。
    pub detail: Option<String>,
    pub span: Span,
    /// 所在 cell 名（cell 内诊断时填；顶层诊断为 `None`）。
    pub cell: Option<String>,
}

impl Diagnostic {
    /// 构造错误诊断。
    pub fn error(code: u16, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            code,
            severity: Severity::Error,
            message: message.into(),
            detail: None,
            span,
            cell: None,
        }
    }

    /// 构造警告诊断。
    pub fn warning(code: u16, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            code,
            severity: Severity::Warning,
            message: message.into(),
            detail: None,
            span,
            cell: None,
        }
    }

    /// 追加细节。
    pub fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }

    /// 标注所在 cell。
    pub fn with_cell(mut self, name: impl Into<String>) -> Self {
        self.cell = Some(name.into());
        self
    }

    /// 渲染为一行文本（§15.1）。
    ///
    /// `RSC-E030: <message>；<detail>；位置: cell <name> @ <line>:<col>`，
    /// 无 detail 省略 `；<detail>` 段；无 cell 名时位置段退化为 `<file_name> @ <line>:<col>`。
    pub fn render(&self, file_name: &str) -> String {
        let mut s = String::new();
        s.push_str("RSC-");
        s.push(self.severity.tag());
        s.push_str(&format!("{:03}", self.code));
        s.push_str(": ");
        s.push_str(&self.message);
        if let Some(detail) = &self.detail {
            s.push('；');
            s.push_str(detail);
        }
        s.push_str("；位置: ");
        match &self.cell {
            Some(name) => {
                s.push_str("cell ");
                s.push_str(name);
            }
            None => s.push_str(file_name),
        }
        s.push_str(&format!(" @ {}:{}", self.span.line, self.span.col));
        s
    }
}

/// 错误码常量表。
///
/// - `E010`..`W060`：**附录 B 原文**（本 crate 只使用其中词法/语法相关的 E030-E033）。
/// - `E034` / `E036` / `E037`：**文档扩展码**（附录 B 未收录，由主会话裁定 A6 补充）：
///   - `E034`：`cell <name>;` 之后必须紧跟 `receive(...)`（§3.4.1 规则 1）。
///   - `E036`：`cell` 名称重复定义。
///   - `E037`：多于一条 `entry` 声明（§2.1 规则 3）。
/// - `E030` 在 parser 中**兼作通用语法错误桶**：附录 B 没有"缺分号 / 期望标识符"一类
///   语法错误码，而主会话裁定 A6 要求"其余错误一律用附录 B 已有码"，故这类消息统一用
///   `E030`，但消息文本以 `语法错误：` 开头，与附录 B 词法场景的 `词法错误：` 区分。
pub mod code {
    // ---- 附录 B 原文 ----
    /// 分支可达但未收敛（check 层使用）。
    #[allow(dead_code)]
    pub const E010: u16 = 10;
    /// jump loop 目标必须为当前 Cell。
    #[allow(dead_code)]
    pub const E011: u16 = 11;
    /// 无法抵达出口的无限 jump loop 路径。
    #[allow(dead_code)]
    pub const E014: u16 = 14;
    /// 缺少程序入口（check 层使用）。
    #[allow(dead_code)]
    pub const E020: u16 = 20;
    /// 词法错误：未闭合的字符串/括号。
    pub const E030: u16 = 30;
    /// 孤立关键字。
    pub const E031: u16 = 31;
    /// 顶层必须是 cell / extern rust / entry / Rust item。
    pub const E032: u16 = 32;
    /// jump 语句解析失败。
    pub const E033: u16 = 33;
    /// 非标识符实参必须显式 `expr -> key`（check 层使用；parser 用 `Arg::effective_key` 暴露依据）。
    #[allow(dead_code)]
    pub const E040: u16 = 40;
    /// jump loop 实参个数与 receive 键数不一致。
    #[allow(dead_code)]
    pub const E041: u16 = 41;
    /// receive 引用了未传入的键。
    #[allow(dead_code)]
    pub const E042: u16 = 42;
    /// 多源并行组 receive 必须使用 `包.变量` 限定。
    #[allow(dead_code)]
    pub const E043: u16 = 43;
    /// 包名/键名不存在于对应子 Cell 输出契约。
    #[allow(dead_code)]
    pub const E044: u16 = 44;
    /// concurrent_limit 依赖检查。
    #[allow(dead_code)]
    pub const E050: u16 = 50;
    /// move / 冻结 / 遮蔽相关（E060..E064, E070）。
    #[allow(dead_code)]
    pub const E060: u16 = 60;
    /// let 绑定遮蔽 receive 别名。
    #[allow(dead_code)]
    pub const E070: u16 = 70;
    /// over 闭包必须是纯函数。
    #[allow(dead_code)]
    pub const E080: u16 = 80;
    /// extern rust 块内禁止 DSL 关键字。
    #[allow(dead_code)]
    pub const E090: u16 = 90;
    /// rustc 编译错误反映射。
    #[allow(dead_code)]
    pub const E900: u16 = 900;
    /// extern rust 块内的 rustc 错误。
    #[allow(dead_code)]
    pub const E901: u16 = 901;
    /// cell 不可达告警。
    #[allow(dead_code)]
    pub const W001: u16 = 1;
    /// force read with 于小 POD 类型告警。
    #[allow(dead_code)]
    pub const W060: u16 = 60;

    // ---- 文档扩展码（附录 B 未收录，见模块文档） ----
    /// `cell` 之后缺少 `receive(...)`。
    pub const E034: u16 = 34;
    /// `cell` 名称重复定义。
    pub const E036: u16 = 36;
    /// 多于一条 `entry` 声明。
    pub const E037: u16 = 37;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_matches_spec_format() {
        let d = Diagnostic::error(code::E030, "词法错误：未闭合的字符串", Span::new(1, 4, 9, 3))
            .with_detail("遇到 EOF")
            .with_cell("fact");
        assert_eq!(d.render("a.rscel"), "RSC-E030: 词法错误：未闭合的字符串；遇到 EOF；位置: cell fact @ 4:9");
    }

    #[test]
    fn render_without_detail_and_cell() {
        let d = Diagnostic::error(code::E032, "顶层必须是 cell / extern rust / entry / Rust item", Span::new(0, 1, 1, 0));
        assert_eq!(d.render("b.rscel"), "RSC-E032: 顶层必须是 cell / extern rust / entry / Rust item；位置: b.rscel @ 1:1");
    }

    #[test]
    fn render_warning_code_is_zero_padded() {
        let d = Diagnostic::warning(code::W001, "cell foo 不可达", Span::new(0, 12, 1, 0));
        assert!(d.render("x.rscel").starts_with("RSC-W001: "));
    }
}
