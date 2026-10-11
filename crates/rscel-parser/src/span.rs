//! 源位置基础设施（规格 §8.1 `span.rs` / §15.1 诊断定位）。
//!
//! 约定（**裁定依据**：§8.1 的 `Span` 只有 `file/line/col/len` 四个字段，没有字节偏移；
//! 而 §10.3 的"透传区重写"与 §15 的源映射都需要字节区间，故约定 `len` 为**字节长度**，
//! `line`/`col` 均 1 起始，`col` 按**字节**计（与 `len` 同单位，便于 `text` 切片回查））：
//! - `Span::join` 只在同行区间上给出精确结果，跨行时 `len` 为近似值（文档缺陷，见交付报告）。

/// 源位置区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span {
    /// 文件 id（由 `parse_program(src, file_id)` 传入）。
    pub file: u32,
    /// 行号，1 起始。
    pub line: u32,
    /// 列号，1 起始，按字节计。
    pub col: u32,
    /// 覆盖的字节长度。
    pub len: u32,
}

impl Span {
    /// 构造一个完整区间。
    pub const fn new(file: u32, line: u32, col: u32, len: u32) -> Self {
        Span { file, line, col, len }
    }

    /// 构造一个零长度（点）区间。
    pub const fn at(file: u32, line: u32, col: u32) -> Self {
        Span { file, line, col, len: 0 }
    }

    /// 结束列（1 起始）。仅在单行区间上有意义。
    pub const fn end_col(&self) -> u32 {
        self.col + self.len
    }

    /// 区间是否为空（零长度或未定位）。
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 合并两个区间，得到覆盖二者的最小起点 + 最远终点。
    ///
    /// 跨行时无法在"行/列"空间内精确表达，故 `len` 仅取"终点列 - 起点列"（下界近似）。
    /// parser 内部所有跨行节点都走 [`span_between`]（有精确字节区间），本方法只用于
    /// 手工拼装的零散 span。
    pub fn join(self, other: Span) -> Span {
        let (line, col) = match (self.line, other.line) {
            (0, 0) => (0, 0),
            (0, l) => (l, other.col),
            (l, 0) => (l, self.col),
            (a, b) if a == b => (a, self.col.min(other.col)),
            (a, b) if a < b => (a, self.col),
            _ => (other.line, other.col),
        };
        let end = if other.line == 0 { self.end_col() } else { other.end_col() };
        Span { file: self.file, line, col, len: end.saturating_sub(col) }
    }
}

/// 字节偏移 -> `(行, 列)`，行列均 1 起始，列按字节计。
///
/// 越界偏移会被夹到 `src.len()`；非字符边界偏移会向内对齐到最近的字符边界
/// （**裁定依据**：所有扫描器都按 `char` 推进，正常路径不会产生非法边界；此处向内对齐
/// 是"绝不 panic"的兜底，对齐本身不改变行号）。
pub fn locate(src: &str, offset: usize) -> (u32, u32) {
    let mut off = offset.min(src.len());
    while off > 0 && !src.is_char_boundary(off) {
        off -= 1;
    }
    let bytes = &src.as_bytes()[..off];
    let mut line: u32 = 1;
    let mut line_start: usize = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            line = line.saturating_add(1);
            line_start = i + 1;
        }
    }
    (line, (off - line_start + 1) as u32)
}

/// 由字节区间 `[start, end)` 构造精确的 [`Span`]（`len` = `end - start` 字节）。
///
/// 同样做字符边界向内对齐，保证 `&src[start..end]` 永不在 UTF-8 字符中间切开。
pub fn span_between(src: &str, file: u32, start: usize, end: usize) -> Span {
    let (s, e) = align_range(src, start, end);
    let (line, col) = locate(src, s);
    Span { file, line, col, len: (e - s) as u32 }
}

/// 把字节区间向内对齐到字符边界（内部工具，`pub(crate)`）。
pub fn align_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let mut s = start.min(src.len());
    let mut e = end.min(src.len());
    if s > e {
        std::mem::swap(&mut s, &mut e);
    }
    while s > 0 && !src.is_char_boundary(s) {
        s -= 1;
    }
    while e < src.len() && !src.is_char_boundary(e) {
        e += 1;
    }
    (s, e)
}

/// 裁掉字节区间两端的 ASCII 空白（**裁定依据**：主会话裁定 A1 "禁止做任何 trim 以外的
/// 加工"——trim 被明确允许）。
///
/// 返回裁剪后的 `(start, end)`。
pub fn trim_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let (s, e) = align_range(src, start, end);
    let slice = &src[s..e];
    let lead = slice.len() - slice.trim_start_matches(is_ws).len();
    let trail = slice.len() - slice.trim_end_matches(is_ws).len();
    (s + lead, e - trail)
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{0b}' | '\u{0c}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_counts_lines_from_one() {
        let src = "cell a;\nreceive(x);\n";
        assert_eq!(locate(src, 0), (1, 1));
        assert_eq!(locate(src, 7), (1, 8));
        assert_eq!(locate(src, 8), (2, 1));
        assert_eq!(locate(src, 10), (2, 3));
    }

    #[test]
    fn locate_is_byte_based_on_multibyte_line() {
        // 注释含中文时列号按字节计（与 len 同单位）
        let src = "// 注释\nlet x = 1;";
        let (line, col) = locate(src, src.find("let").unwrap());
        assert_eq!(line, 2);
        assert_eq!(col, 1);
    }

    #[test]
    fn span_between_multiline_keeps_byte_len() {
        let src = "if c {\n  die;\n}\n";
        let start = 7;
        let end = src.find("die").unwrap();
        let s = span_between(src, 1, start, end);
        assert_eq!((s.line, s.col), (2, 1));
        assert_eq!(s.len as usize, end - start);
    }

    #[test]
    fn join_same_line_is_exact() {
        let a = Span::new(1, 3, 5, 4);
        let b = Span::new(1, 3, 10, 2);
        let j = a.join(b);
        assert_eq!(j, Span::new(1, 3, 5, 7));
    }

    #[test]
    fn join_uses_min_start() {
        let a = Span::new(1, 9, 1, 3);
        let b = Span::new(1, 2, 4, 3);
        assert_eq!(a.join(b), Span::new(1, 2, 4, 3));
    }

    #[test]
    fn align_range_never_splits_char() {
        let src = "中文abc";
        let (s, e) = align_range(src, 1, 5);
        assert!(src.is_char_boundary(s) && src.is_char_boundary(e));
        assert_eq!(&src[s..e], "中文", "两端向内对齐到字符边界");
    }
}
