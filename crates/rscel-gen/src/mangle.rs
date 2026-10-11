//! 标识符 mangle（§12.7）。

/// Cell 名 → Rust 函数名 `cell_<sanitized>`。
///
/// 合法化规则：保留 `[a-z0-9_]`，其余字符替换为 `_`；首字符必须小写或下划线
/// （防止 `Cell123` → `cell_Cell123` 与 Rust 类型名冲突）。
pub fn cell_fn_name(name: &str) -> String {
    let mut out = String::from("cell_");
    let mut first = true;
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' };
        if first {
            if c.is_ascii_digit() {
                out.push('_');
            }
            first = false;
        }
        out.push(c);
    }
    out
}

/// Cell 名 → Ret 结构体名 `Ret<PascalCase(sanitized)>`。
///
/// UpperCamelCase 是 Rust 类型名规范（避免 clippy non_camel_case_types）。同样做
/// 字符合法化，把首字符改为大写，避免下划线分隔（让 `Ret_fact`/`Ret_main` 看起来
/// 像模块路径而非类型名）。
pub fn ret_struct_name(name: &str) -> String {
    let mut out = String::from("Ret");
    let mut first = true;
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' };
        if first {
            let up = if c.is_ascii_digit() { '_' } else { c.to_ascii_uppercase() };
            out.push(up);
            first = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_fn_name_basic() {
        assert_eq!(cell_fn_name("fact"), "cell_fact");
        assert_eq!(cell_fn_name("main"), "cell_main");
        assert_eq!(cell_fn_name("Foo-Bar"), "cell_Foo_Bar");
        assert_eq!(cell_fn_name("123abc"), "cell__123abc");
    }

    #[test]
    fn ret_struct_name_basic() {
        assert_eq!(ret_struct_name("fact"), "RetFact");
        assert_eq!(ret_struct_name("main"), "RetMain");
        assert_eq!(ret_struct_name("worker1"), "RetWorker1");
    }
}
