//! 标识符重写（§10.3）。
//!
//! 在 Cell body 的 RawRust 透传文本里，把**当前 Cell 的 receive 别名**替换为
//! `v_<id>`；其他标识符（含 `let` 绑定的局部变量）一律不动。
//!
//! **必须**做精细扫描的词法单元（P1-4 修了以前是 bug）：
//!   - 字符串字面量 `"..."` 与字节串 `b"..."`：内容整体透传，**不重写**；
//!   - 原始字符串 `r"..."` / `r#"..."#` / `br"..."` 等：同上；
//!   - 字符字面量 `'.'`（含 `\` 转义）：同上；
//!   - 生命周期 `'ident`：不会被误认为进来（`'` 不是标识符起始），但为对称起见
//!     也走同一路径；
//!   - 行注释 `//...`    + 块注释 `/* ... */`：透传；
//!   - 数字字面量 `123`、`0xFF`、`1.0e10`：透传。
//!
//! 参考 rscel-parser §3.5.3 的状态机实现思路（`skip_string` / `skip_raw_string` /
//! `skip_block_comment` / `skip_char_or_lifetime` / `skip_number`），
//! 但本函数是「原样复制 + 必要时改写」而不是「跳过」，所以单独实现。
//!
//! P1-4 真实触发路径（修复前静默错误）：
//!   receive 别名 k  ─┐
//!   `exit go with("k" -> result)` ─ rewrite ─> `exit go with("v_a" -> result)`
//! 类型不变、rustc 零告警、语义已变（结构体里 result: v_a 而不是 "k"）。

/// 把 receive 别名集合映射到 `v_<id>` 的改写函数。
///
/// `alias_to_var`：`&str`（别名）→ `&str`（形如 `v_a` / `v_b` …，由 caller 按 §10.4 的
/// 26 进制 VarId 算法生成）。
pub fn rewrite(raw_text: &str, alias_to_var: &[(String, String)]) -> String {
    if alias_to_var.is_empty() {
        return raw_text.to_string();
    }
    let mut out = String::with_capacity(raw_text.len());
    let bytes = raw_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        // ---- 行注释 //...<newline> ----
        if b == b'/' && peek1(bytes, i) == Some(b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }
        // ---- 块注释 /* ... */ ----
        if b == b'/' && peek1(bytes, i) == Some(b'*') {
            out.push('/');
            out.push('*');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i + 1 < bytes.len() {
                out.push('*');
                out.push('/');
                i += 2;
            }
            continue;
        }

        // ---- b"..." / b'..' / br"..." ----
        if b == b'b' {
            if peek1(bytes, i) == Some(b'"') {
                out.push('b');
                i += 1;
                copy_str_literal(bytes, &mut i, &mut out);
                continue;
            }
            if peek1(bytes, i) == Some(b'\'') {
                out.push('b');
                i += 1;
                copy_char_or_lifetime(bytes, &mut i, &mut out);
                continue;
            }
            // br"..." / br#"..."#
            if peek1(bytes, i) == Some(b'r')
                && matches!(peek2(bytes, i), Some(c) if c == b'"' || c == b'#')
            {
                copy_byte_raw_string(bytes, &mut i, &mut out);
                continue;
            }
        }

        // ---- r"..." / r#"..."# / r##"..."## ----
        if b == b'r' && matches!(peek1(bytes, i), Some(c) if c == b'"' || c == b'#') {
            copy_raw_string(bytes, &mut i, &mut out);
            continue;
        }

        // ---- 普通字符串 "..." ----
        if b == b'"' {
            copy_str_literal(bytes, &mut i, &mut out);
            continue;
        }

        // ---- 字符字面量 '.' / 生命周期 'ident ----
        if b == b'\'' {
            copy_char_or_lifetime(bytes, &mut i, &mut out);
            continue;
        }

        // ---- 数字字面量 ----
        if b.is_ascii_digit() {
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // ---- 标识符（含别名）----
        if is_ident_start(b) {
            let start = i;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let token = &raw_text[start..i];
            if let Some((_, repl)) = alias_to_var.iter().find(|(a, _)| *a == token) {
                out.push_str(repl);
            } else {
                out.push_str(token);
            }
            continue;
        }

        // ---- 默认：原样复制 ----
        out.push(b as char);
        i += 1;
    }
    out
}

#[inline]
fn peek1(bytes: &[u8], i: usize) -> Option<u8> {
    bytes.get(i + 1).copied()
}

#[inline]
fn peek2(bytes: &[u8], i: usize) -> Option<u8> {
    bytes.get(i + 2).copied()
}

/// 复制一段普通字符串 `"..."`（含转义）到 out；`i` 落在开 `"` 上，
/// 结束时 `i` 落在闭 `"` 之后或末尾 / 换行（未闭合）。
fn copy_str_literal(bytes: &[u8], i: &mut usize, out: &mut String) {
    debug_assert_eq!(bytes[*i], b'"');
    out.push('"');
    *i += 1;
    while *i < bytes.len() {
        let b = bytes[*i];
        if b == b'\\' && *i + 1 < bytes.len() {
            out.push(b as char);
            out.push(bytes[*i + 1] as char);
            *i += 2;
            continue;
        }
        if b == b'"' {
            out.push('"');
            *i += 1;
            return; // 见 if 后的 push —— 不可去掉这个 return
        }
        if b == b'\n' {
            return; // 未闭合 —— 不消费它，留给上层主循环。
        }
        out.push(b as char);
        *i += 1;
    }
}

/// 复制一段原始字符串 `r"..."` / `r#"..."#` / `r##"..."##` 到 out。
/// `i` 落在 `r` 上。
fn copy_raw_string(bytes: &[u8], i: &mut usize, out: &mut String) {
    debug_assert_eq!(bytes[*i], b'r');
    out.push('r');
    *i += 1;
    let mut hashes = 0;
    while *i < bytes.len() && bytes[*i] == b'#' {
        out.push('#');
        hashes += 1;
        *i += 1;
    }
    if *i >= bytes.len() || bytes[*i] != b'"' {
        return; // 不是原始字符串（主循环下次按标识符处理）
    }
    out.push('"');
    *i += 1;
    while *i < bytes.len() {
        let b = bytes[*i];
        if b == b'"' {
            let mut k = 0;
            while k < hashes && *i + 1 + k < bytes.len() && bytes[*i + 1 + k] == b'#' {
                k += 1;
            }
            if k == hashes {
                out.push('"');
                *i += 1;
                for _ in 0..hashes {
                    out.push('#');
                    *i += 1;
                }
            }
        }
        out.push(b as char);
        *i += 1;
    }
}

/// 复制一段字节原始字符串 `br"..."` / `br#"..."#` 到 out。`i` 落在 `b` 上。
fn copy_byte_raw_string(bytes: &[u8], i: &mut usize, out: &mut String) {
    debug_assert_eq!(bytes[*i], b'b');
    debug_assert_eq!(bytes[*i + 1], b'r');
    out.push('b');
    out.push('r');
    *i += 2;
    let mut hashes = 0;
    while *i < bytes.len() && bytes[*i] == b'#' {
        out.push('#');
        hashes += 1;
        *i += 1;
    }
    if *i >= bytes.len() || bytes[*i] != b'"' {
        return;
    }
    out.push('"');
    *i += 1;
    while *i < bytes.len() {
        let b = bytes[*i];
        if b == b'"' {
            let mut k = 0;
            while k < hashes && *i + 1 + k < bytes.len() && bytes[*i + 1 + k] == b'#' {
                k += 1;
            }
            if k == hashes {
                out.push('"');
                *i += 1;
                for _ in 0..hashes {
                    out.push('#');
                    *i += 1;
                }
            }
        }
        out.push(b as char);
        *i += 1;
    }
}

/// 复制字符字面量 `'.'`（含转义）或生命周期 `'ident` 到 out。
/// `i` 落在 `'` 上。
///
/// 区分两者的关键是：字符字面量是 `'X'`（'X' 后紧跟闭 `'`），
/// 生命周期是 `'ident`（ident 后是上下文终结符 `>` / `,` / 空格 / 等）。
/// 简单的「扫到下一个 `'`」会让生命周期跨到下一个 `'`（如 `&'cur` 误吃
/// 到下一个 lifetime 的开 `'`），所以本版本用 ident 边界判定。
fn copy_char_or_lifetime(bytes: &[u8], i: &mut usize, out: &mut String) {
    debug_assert_eq!(bytes[*i], b'\'');
    out.push('\'');
    *i += 1;
    if *i >= bytes.len() {
        return;
    }
    let b = bytes[*i];

    // 转义序列：'\\n'、'\\xFF'、'\\u{...}' 等。
    if b == b'\\' {
        out.push(b as char);
        *i += 1;
        if *i < bytes.len() {
            out.push(bytes[*i] as char);
            *i += 1;
        }
        if *i < bytes.len() && bytes[*i] == b'\'' {
            out.push('\'');
            *i += 1;
        }
    }

    // 复制 ident 字节（生命周期或单字符 char literal 的 ident 部分）。
    while *i < bytes.len() && is_ident_cont(bytes[*i]) {
        out.push(bytes[*i] as char);
        *i += 1;
    }
    // 若紧跟闭 `'`，说明是字符字面量：闭合即可。
    if *i < bytes.len() && bytes[*i] == b'\'' {
        out.push('\'');
        *i += 1;
    }
    // 否则是生命周期（如 'cur 或 'static）：ident 已逐字复制，
    // 下一个 token（`>` / `,` / 空格 等）由主循环按默认路径处理。
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// §10.4 的 VarId 26 进制编码。
pub fn var_id(n: usize) -> String {
    // n=0→"a", 1→"b", ..., 25→"z", 26→"aa", 27→"ab", ..., 701→"zz", 702→"aaa"
    let mut s = String::new();
    let mut n = n;
    loop {
        s.insert(0, (b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 { break; }
        n -= 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_alias_to_var() {
        let map = vec![("cur".to_string(), "v_a".to_string()), ("sum".to_string(), "v_b".to_string())];
        assert_eq!(rewrite("let x = cur - 1;", &map), "let x = v_a - 1;");
        assert_eq!(rewrite("let y = sum * cur;", &map), "let y = v_b * v_a;");
        let map2 = vec![("cur".to_string(), "v_a".to_string())];
        assert_eq!(rewrite("next_acc", &map2), "next_acc"); // 别名未命中，原样
    }

    /// P1-4 修复后：字符串字面量必须透传，**不**改写内部标识符。
    /// 行/块注释、数字、字符字面量、原始字符串、生命周期同理。
    #[test]
    fn rewrite_preserves_strings_and_idents() {
        // map 同时含 `cur` 和 `sum`，方便测试「注释里不重写，但代码里重写」。
        let map = vec![
            ("cur".to_string(), "v_a".to_string()),
            ("sum".to_string(), "v_b".to_string()),
        ];
        assert_eq!(rewrite(r#"let s = "cur";"#, &map), r#"let s = "cur";"#);
        assert_eq!(
            rewrite(r#"let s = "cur is cur and cur";"#, &map),
            r#"let s = "cur is cur and cur";"#
        );
        assert_eq!(
            rewrite(r#"let s = b"cur";"#, &map),
            r#"let s = b"cur";"#
        );
        assert_eq!(
            rewrite(r##"let s = r#"cur"  (a);"##, &map),
            r##"let s = r#"cur"  (a);"##
        );
        assert_eq!(
            rewrite("// cur comment\nlet x = sum;", &map),
            "// cur comment\nlet x = v_b;"
        );
        assert_eq!(
            rewrite("/* cur */ let x = sum;", &map),
            "/* cur */ let x = v_b;"
        );
        assert_eq!(rewrite("let x = 12cur;", &map), "let x = 12cur;");
        assert_eq!(
            rewrite(r"let x = '\n' + cur;", &map),
            r"let x = '\n' + v_a;"
        );
        assert_eq!(
            rewrite("fn f<'cur>(x: &'cur i64) {}", &map),
            "fn f<'cur>(x: &'cur i64) {}"
        );
    }

    #[test]
    fn var_id_26_base() {
        assert_eq!(var_id(0), "a");
        assert_eq!(var_id(25), "z");
        assert_eq!(var_id(26), "aa");
        assert_eq!(var_id(27), "ab");
        assert_eq!(var_id(701), "zz");
        assert_eq!(var_id(702), "aaa");
    }
}
