//! 手写 JSON 写出器 + 表单（`application/x-www-form-urlencoded`）解码。
//!
//! ## 为什么自己写
//!
//! 本 workspace 的硬规约是**零外部依赖**（只用 `std`），而 `std` 没有 JSON。
//! `rscel edit` 的 HTTP 后端要把诊断、拓扑、trace 步骤回传给浏览器，浏览器侧
//! 用 `JSON.parse` 消费，所以必须有一个能产出合法 JSON 的写出器；同时前端用
//! `URLSearchParams` 发 `source=<整段源码>`，所以必须有一个 percent-decode。
//! 这两个加起来就是本模块的全部职责——**只写需要的那一面**，不做通用序列化。
//!
//! ## 为什么输出带缩进
//!
//! 前端 `JSON.parse` 对缩进无所谓，但 HTTP 排障时人要看 `curl` 的返回；
//! 缩进让「编辑器里看到的」和「终端里看到的」是同一份文本，不必再管道给 jq
//! （本机也没有 jq 之外的工具保证）。
//!
//! ## 转义规则（刻意与 `serde_json` 的默认行为不同）
//!
//! - `"`、`\`、`\n`、`\r`、`\t`、以及所有 `< 0x20` 的控制符必须转义（JSON 硬性要求）；
//! - **非 ASCII（中文）按 UTF-8 原样写出**，不转 `\uXXXX`：本项目的诊断文本、
//!   DSL 注释全是中文，转义后 `curl` 输出不可读，而且 `Content-Length` 要按字节算，
//!   原样 UTF-8 让「长度 = 字节数」这条心算保持简单。
//!
//! [`unescape_string`] 只服务于本模块的往返自校验（HTTP 侧只读表单，不读 JSON）。

use std::fmt::Write as _;

/// 一个 JSON 值。
///
/// 只保留本项目用得上的六种形态；`Obj` 用 `Vec` 而不是 `HashMap`，
/// 因为**字段顺序必须稳定**（前端 diff、测试断言、人眼对比都依赖顺序）。
#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// 构造一个对象字段（`Json::object(vec![field("code", Json::Int(3))])`）。
pub fn field(name: &str, value: Json) -> (String, Json) {
    (name.to_string(), value)
}

impl Json {
    /// 字符串（`&str` 版）。
    pub fn text(s: &str) -> Json {
        Json::Str(s.to_string())
    }

    /// 整数（诊断码、行号、step 序号都是整数，没有浮点）。
    pub fn int(i: i64) -> Json {
        Json::Int(i)
    }

    /// 布尔（`died` / `is_entry` / `ok`）。
    pub fn bool(b: bool) -> Json {
        Json::Bool(b)
    }

    /// 字段保序的对象。
    pub fn object(fields: Vec<(String, Json)>) -> Json {
        Json::Obj(fields)
    }

    /// 数组。
    pub fn arr(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }

    pub fn opt_str(o: &Option<String>) -> Json {
        match o {
            Some(s) => Json::Str(s.clone()),
            None => Json::Null,
        }
    }

    /// `Option<Json>` → 值或 `Null`（前端契约里「没有该字段」一律给 `null`，
    /// 因为 `undefined` 在 JSON 里无法表达，缺字段会让前端写一堆 `||`）。
    pub fn opt(o: Option<Json>) -> Json {
        o.unwrap_or(Json::Null)
    }

    pub fn strs(items: &[String]) -> Json {
        Json::Arr(items.iter().map(|s| Json::Str(s.clone())).collect())
    }

    /// 缩进 2 空格的可读文本。
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self, 0);
        out
    }

    /// 单行紧凑文本（错误消息里嵌 JSON 时用，避免换行把一行日志劈成两段）。
    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        write_value_compact(&mut out, self);
        out
    }
}

fn write_value(out: &mut String, v: &Json, indent: usize) {
    let pad = "  ".repeat(indent);
    let pad_inner = "  ".repeat(indent + 1);
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Str(s) => write_escaped(out, s),
        Json::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad_inner);
                write_value(out, item, indent + 1);
                out.push_str(if i + 1 == items.len() { "\n" } else { ",\n" });
            }
            out.push_str(&pad);
            out.push(']');
        }
        Json::Obj(fields) => {
            if fields.is_empty() {
                // `{}` 而不是 `{\n}`：空对象在 `groups: []` 之类容器里很常见，
                // 拆成两行会让整棵树白白长一倍。
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, val)) in fields.iter().enumerate() {
                out.push_str(&pad_inner);
                write_escaped(out, k);
                out.push_str(": ");
                write_value(out, val, indent + 1);
                out.push_str(if i + 1 == fields.len() { "\n" } else { ",\n" });
            }
            out.push_str(&pad);
            out.push('}');
        }
    }
}

fn write_value_compact(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Str(s) => write_escaped(out, s),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                write_value_compact(out, item);
            }
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                write_escaped(out, k);
                out.push(':');
                write_value_compact(out, val);
            }
            out.push('}');
        }
    }
}

/// 写一个 JSON 字符串字面量（含首尾引号）。
pub fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // 其余控制符（含 \u{0b}/\u{0c}，即 JS 的 \v \f）按 §ECMA-404 用
            // \u00XX；这里是「其它 <0x20」的兜底分支，所以不再单列 \b \f。
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            // 非 ASCII 原样输出（见模块注释）
            other => out.push(other),
        }
    }
    out.push('"');
}

/// 解析一个 JSON 字符串字面量（含首尾引号）——只用于本模块的往返自校验。
///
/// 支持 `\" \\ \/ \n \r \t \b \f \uXXXX`；`\uXXXX` 遇代理对时组合成一个码点。
pub fn unescape_string(literal: &str) -> Result<String, String> {
    let bytes = literal.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Err("字符串字面量必须用双引号包裹".to_string());
    }
    let src = &literal[1..literal.len() - 1];
    let mut out = String::new();
    let mut it = src.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hi = read_hex4(&mut it)?;
                let ch = if (0xD800..0xDC00).contains(&hi) {
                    // 前导代理：必须跟 `\uXXXX` 后导代理，否则是坏 JSON。
                    if it.next() != Some('\\') || it.next() != Some('u') {
                        return Err("\\u 代理对缺少后导代理".to_string());
                    }
                    let lo = read_hex4(&mut it)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err("\\u 代理对的后导码点非法".to_string());
                    }
                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else {
                    hi
                };
                out.push(char::from_u32(ch).ok_or_else(|| format!("\\u{:04X} 不是合法码点", ch))?);
            }
            Some(other) => return Err(format!("非法转义 \\{other}")),
            None => return Err("反斜杠在结尾被截断".to_string()),
        }
    }
    Ok(out)
}

fn read_hex4(it: &mut impl Iterator<Item = char>) -> Result<u32, String> {
    let mut v: u32 = 0;
    for _ in 0..4 {
        let c = it.next().ok_or_else(|| "\\u 需要 4 位十六进制".to_string())?;
        v = v * 16 + c.to_digit(16).ok_or_else(|| format!("\\u 里出现非十六进制字符 {c}"))?;
    }
    Ok(v)
}

// ---------------------------------------------------------------- 表单解码

/// percent-decode：`%XX` 按**字节**还原，连续多字节自然组成 UTF-8 序列；`+` → 空格。
///
/// 非法序列（`%ZZ`、截断的 `%E4`、以及解码后不是合法 UTF-8 的字节串）返回 `Err`，
/// **不 panic**：HTTP 的输入全是外部可控，这里 panic 就等于让服务崩掉。
pub fn percent_decode(raw: &str) -> Result<String, String> {
    let b = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() {
                    return Err(format!("percent 序列被截断: {:?}", &raw[i..]));
                }
                let hi = char::from(b[i + 1]).to_digit(16).ok_or_else(|| "percent 序列非法（非十六进制）".to_string())?;
                let lo = char::from(b[i + 2]).to_digit(16).ok_or_else(|| "percent 序列非法（非十六进制）".to_string())?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| format!("percent 解码后不是合法 UTF-8: {e}"))
}

/// 解 `application/x-www-form-urlencoded`。
///
/// 只按第一个 `=` 切分（值里的 `=` 必须保留，DSL 的 `concurrent_limit=2` 就带 `=`），
/// 空片段跳过（浏览器常在结尾留一个 `&`）。
pub fn form_decode(body: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for part in body.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (percent_decode(k)?, percent_decode(v)?),
            None => (percent_decode(part)?, String::new()),
        };
        pairs.push((k, v));
    }
    Ok(pairs)
}

/// 取第一个同名键的值（重复键取先出现的：前端只发一份 source）。
pub fn form_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// 把值编码成表单字段（给测试与 CLI 造请求体用；与 [`percent_decode`] 互逆）。
pub fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(*b as char),
            b' ' => out.push('+'),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj_str(s: &str) -> String {
        Json::object(vec![field("文本", Json::text(s))]).to_pretty()
    }

    #[test]
    fn chinese_is_written_as_raw_utf8_not_unicode_escape() {
        let out = obj_str("阶乘 中文");
        assert!(out.contains("阶乘 中文"), "非 ASCII 必须原样输出: {out}");
        assert!(!out.contains("\\u"), "不得出现 \\uXXXX 转义: {out}");
        // 中文按 UTF-8 是 3 字节/字，字段名同样是中文（顺序稳定）。
        assert!(out.contains("\"文本\""), "{out}");
    }

    #[test]
    fn escapes_are_round_tripped() {
        let original = "含\"引号\"、反斜杠 \\、换行\n回车\r制表\t以及 中文 与 控制符\u{1}";
        let mut lit = String::new();
        write_escaped(&mut lit, original);
        assert!(lit.contains("\\n"), "{lit}");
        assert!(lit.contains("\\u0001"), "控制符必须走 \\u00XX: {lit}");
        assert_eq!(unescape_string(&lit).expect("往返必须成功"), original);
    }

    #[test]
    fn pretty_output_is_escaped_and_indented() {
        let v = Json::object(vec![
            field("file", Json::text("a\r\nb.rscel")),
            field("steps", Json::Arr(vec![Json::Int(1), Json::Int(2)])),
            field("note", Json::Null),
            field("ok", Json::Bool(true)),
        ]);
        let text = v.to_pretty();
        assert!(text.contains("\"file\": \"a\\r\\nb.rscel\""), "{text}");
        assert!(text.contains("\"note\": null"), "{text}");
        assert!(text.contains("\"ok\": true"), "{text}");
        // 数组元素各自成行（缩进可读）
        assert!(text.contains("    1,"), "{text}");
    }

    #[test]
    fn empty_containers_stay_inline() {
        assert_eq!(Json::object(Vec::new()).to_pretty(), "{}");
        assert_eq!(Json::Arr(Vec::new()).to_pretty(), "[]");
    }

    #[test]
    fn compact_matches_pretty_after_parse() {
        let v = Json::object(vec![field("k", Json::text("v"))]);
        assert_eq!(v.to_compact(), r#"{"k":"v"}"#);
    }

    #[test]
    fn percent_decode_handles_utf8_and_plus_and_space() {
        assert_eq!(percent_decode("%E4%B8%AD").unwrap(), "中");
        assert_eq!(percent_decode("a%20b").unwrap(), "a b");
        assert_eq!(percent_decode("a+b").unwrap(), "a b");
        assert_eq!(percent_decode("100%25").unwrap(), "100%");
        assert_eq!(percent_decode("").unwrap(), "");
    }

    #[test]
    fn percent_decode_rejects_malformed_sequences() {
        // 截断的多字节序列：`%E4%B8` 缺第三字节 → UTF-8 重组失败
        assert!(percent_decode("%E4%B8").is_err(), "截断的 UTF-8 序列必须报错");
        // 非十六进制
        assert!(percent_decode("%ZZ").is_err());
        // 结尾只剩一个字符
        assert!(percent_decode("ab%4").is_err());
    }

    #[test]
    fn form_decode_splits_on_first_equals_only() {
        let body = "source=jump%20and%20w%20concurrent_limit%3D2&maxItems=4&empty=";
        let pairs = form_decode(body).expect("必须解析成功");
        assert_eq!(form_get(&pairs, "maxItems"), Some("4"));
        assert_eq!(form_get(&pairs, "empty"), Some(""));
        // 值里的 `=` 必须原样保留（DSL 的 concurrent_limit=2 就靠它）
        assert_eq!(form_get(&pairs, "source"), Some("jump and w concurrent_limit=2"));
    }

    #[test]
    fn form_encode_decode_roundtrip_with_dsl_source() {
        let src = "cell main;\nreceive(mode: i64);\ndie;  // 中文注释\n";
        let encoded = form_encode(src);
        let pairs = form_decode(&format!("source={encoded}")).expect("往返必须成功");
        assert_eq!(form_get(&pairs, "source").unwrap(), src);
    }

    #[test]
    fn duplicate_keys_take_the_first() {
        let pairs = form_decode("a=1&a=2").unwrap();
        assert_eq!(form_get(&pairs, "a"), Some("1"));
        assert_eq!(form_get(&pairs, "missing"), None);
    }
}
