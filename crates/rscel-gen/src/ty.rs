//! D2 接收类型来源：**注解优先；否则从入边实参做局部类型传播；推不出则报错要求注解**。
//!
//! 依据《RSupercell开发说明》§3.4.1（`receive` 允许无注解）与 `docs/00-内存模型与选型报告.md`
//! 的 **D2** 裁定。S2（静态类型化调用约定）决定了 Cell 签名必须写具体类型：
//! `async fn cell_a(x: <具体类型>) -> Option<Ret_a>`。而 Rust 的 item 签名**不允许** `_`
//! 占位（E0121），所以本模块是 S2 的必要前提，而非可选增强。
//!
//! 推导顺序（先命中先用）：
//! 1. `receive(<key>: <ty>)` 的**显式注解**——永远优先，不参与传播；
//! 2. **入边实参**传播：`jump a go with(<expr> -> k)` 决定 `cell a` 的键 `k` 的类型；
//! 3. 用户写在表达式上的消歧 `<expr> as <ty>`；
//! 4. **`extern rust` 里声明的 `fn f(..) -> T`**：调用点 `f(..)` 的类型就是签名写的 `T`
//!    （与规则 1 同性质——都是用户已经写下的权威注解，只是书写位置在 Rust 侧）；
//! 5. 表达式字面量 / 构造器 / 运算符的**字面推导**；
//! 6. Cell 内 `let` 绑定的右值递归。
//!
//! **推不出时返回 `None`，由生成阶段发出 `compile_error!` 要求用户加注解**——
//! 绝不猜一个类型：猜错会静默改变语义（`i64` ↔ `u64` ↔ `f64`），比报错危险得多。
//! 规则 4 的实现因此采取「认不出就不登记」的姿态：签名扫描只在**完全确定**返回类型
//! 文本时才登记，同名 fn 出现不同返回类型记为歧义并一律回落。

use std::collections::HashMap;

use rscel_parser::{CellDef, Ident, JumpKind, Program, Stmt};

/// 程序级类型环境。
///
/// `declared[(cell, key)]` = 目标 Cell 某个 receive 键的 Rust 类型文本。
/// `extern_fns[名]` = 顶层 `extern rust{}` / 顶层 Rust item 里声明的 `fn` 的返回类型。
#[derive(Debug, Default)]
pub struct TypeEnv {
    declared: HashMap<(Ident, Ident), String>,
    extern_fns: HashMap<String, ExternRet>,
}

/// 一个 fn 名在可见范围内解析出的返回类型。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ExternRet {
    /// Rust 类型文本（唯一，或多个同签名声明的一致结果）。
    Known(String),
    /// 同名 fn 携带**不同**的返回类型：调用点究竟指向哪一个无法判定 → 不猜。
    Ambiguous,
}

impl TypeEnv {
    /// 查询 `cell` 的 receive 键 `key`（**键名**，非别名）的类型。
    pub fn declared_type(&self, cell: &str, key: &str) -> Option<&str> {
        self.declared.get(&(cell.to_string(), key.to_string())).map(String::as_str)
    }

    /// 一个 Cell 的 receive 别名 → 键名映射。
    ///
    /// `receive(a, b) as (x, y)` → `x→a`、`y→b`（§3.4.1 规则 2：省略 `as` 时别名 = 键名）。
    fn alias_to_key(cell: &CellDef) -> HashMap<&str, &str> {
        let mut m = HashMap::new();
        for (i, key) in cell.receive.keys.iter().enumerate() {
            // 包限定键 `g1.W1.A` 在裸键传播里按裸名 `A` 匹配（§9.4 的槽位解析在 check 层）。
            let alias: &str = match cell.receive.aliases.get(i) {
                Some(a) => a.as_str(),
                None => key.name.as_str(),
            };
            m.insert(alias, key.name.as_str());
        }
        m
    }

    /// 调用点可见的 `fn <name>` 的返回类型（D2 规则 4）。
    ///
    /// 本 Cell 内的声明**优先**：Rust 的词法作用域里内层 `fn` 遮蔽同名外层 item，
    /// 所以先查本 Cell 的透传 / `extern rust` 块文本，再查程序顶层。
    /// 任一侧歧义都返回 `None`（要求用户改用 `as <类型>` 或加 receive 注解）。
    fn fn_return_type(&self, cell: &CellDef, name: &str) -> Option<String> {
        match cell_fn_returns(cell).get(name) {
            Some(ExternRet::Known(ty)) => return Some(ty.clone()),
            Some(ExternRet::Ambiguous) => return None,
            None => {}
        }
        match self.extern_fns.get(name) {
            Some(ExternRet::Known(ty)) => Some(ty.clone()),
            _ => None,
        }
    }

    /// 在 `cell` 的上下文里推导表达式 `text` 的 Rust 类型。
    pub fn resolve_in(&self, cell: &CellDef, text: &str) -> Option<String> {
        let t = text.trim();

        // 1) 命中本 Cell 的 receive 别名/键名 → 查环境（注解或入边传播的结果）
        if rscel_parser::ast::is_plain_ident(t) {
            let a2k = Self::alias_to_key(cell);
            if let Some(key) = a2k.get(t) {
                if let Some(ty) = self.declared_type(&cell.name, key) {
                    return Some(ty.to_string());
                }
            }
        }

        // 2) 括号包裹：`(a + b)` → 递归
        if let Some(inner) = strip_outer_parens(t) {
            return self.resolve_in(cell, inner);
        }

        // 3) 用户主动消歧：`<表达式> as <类型>`（这也是类型错误提示里建议的写法）
        if let Some((_, rhs)) = t.rsplit_once(" as ") {
            let rhs = rhs.trim();
            if is_rust_type_name(rhs) {
                return Some(rhs.to_string());
            }
        }

        // 3.5) 调用点 `f(<args>)`：`f` 在可见的 `extern rust` / 透传文本里有 `fn f(..) -> T`
        //      声明时，类型就是签名上写明的 `T`。
        if let Some(name) = call_target(t) {
            if let Some(ty) = self.fn_return_type(cell, name) {
                return Some(ty);
            }
        }
        // 4) 字面量后缀：`5i64` / `0.5f64`
        if let Some(ty) = literal_suffix_type(t) {
            return Some(ty);
        }

        // 5) 字面量与构造器
        if let Some(ty) = literal_type(t) {
            return Some(ty);
        }

        // 6) `Arc::new(<x>)` / `Rc::new(<x>)` → `Arc<T>` / `Rc<T>`
        if let Some(inner) = wrapper_call(t, "Arc::new") {
            return self.resolve_in(cell, inner).map(|t| format!("Arc<{t}>"));
        }
        if let Some(inner) = wrapper_call(t, "Rc::new") {
            return self.resolve_in(cell, inner).map(|t| format!("Rc<{t}>"));
        }

        // 6) `Vec::new()` → 元素类型未知（无注解时报错要求用户写明）；
        //     `vec![a, b, ..]` → 由首元素推 `Vec<T>`
        if t == "Vec::new()" {
            return None;
        }
        if let Some(elems) = slice_literal(t) {
            let first = elems.first()?.trim();
            return self.resolve_in(cell, first).map(|t| format!("Vec<{t}>"));
        }

        // 6b) 常见构造器 / 方法调用
        if t == "String::new()" || t == "String::default()" || t.starts_with("String::from(") {
            return Some("String".into());
        }
        if t.ends_with(".to_string()") || t.ends_with(".to_owned()") {
            return Some("String".into());
        }
        if t.ends_with(".len()") {
            return Some("usize".into());
        }
        if let Some(recv) = t.strip_suffix(".clone()") {
            // `x.clone()` 与 `x` 同型（对 Copy/POD 与堆类型都成立）。
            return self.resolve_in(cell, recv.trim());
        }

        // 7) 运算符：先判比较（bool），再判算术（i64）
        if let Some(ty) = operator_type(t) {
            return Some(ty.to_string());
        }

        // 7.5) Cell 内 **带注解** 的 `let <name>: <T> = ...;` —— 注解优先（D2 第一规则）。
        //
        // 修前只走第 8 步的 RHS 递归：`let y: i64 = rsc_slow(x).await;` 的 RHS 是
        // 任意 Rust 表达式，推导器看不懂 → 直接 `compile_error!` 要求用户再写一遍
        // `y as i64`。用户已经写了权威注解，推导器却把它跳过去猜，这是本末倒置。
        if rscel_parser::ast::is_plain_ident(t) {
            if let Some(ty) = find_let_annotation(cell, t) {
                return Some(ty);
            }
        }

        // 8) Cell 内 `let <name> = <rhs>;` 递归
        if let Some(rhs) = find_let_rhs(cell, t) {
            return self.resolve_in(cell, &rhs);
        }

        None
    }

    /// 由全程序入边实参传播类型，得到最终环境。
    ///
    /// 传播是**不动点迭代**：入边实参本身可能是另一个 Cell 的无注解形参
    /// （`cell a: receive(x);` ← `cell b: receive(x); jump a go with(x -> y);`），
    /// 单遍会漏。类型图无环时最多 |cells| 轮收敛，故设上界防死循环。
    pub fn build(program: &Program) -> TypeEnv {
        // 种子：所有显式注解（§3.4.1 的 `receive(<key>: <ty>)`）。
        let mut env = TypeEnv::default();
        for cell in program.cells() {
            for key in &cell.receive.keys {
                if let Some(ty) = &key.ty {
                    env.declared
                        .insert((cell.name.clone(), key.name.clone()), ty.text.trim().to_string());
                }
            }
        }

        // 种子之二：顶层 `extern rust {}` / 顶层 Rust item 里声明的 fn 返回类型（规则 4）。
        // 顶层 item 对**所有** Cell 可见，所以进程序级表；Cell 内的 fn 按需单独扫
        // （见 [`TypeEnv::fn_return_type`]），不并进这张表，否则会跨作用域误采纳。
        for r in program.global_rust() {
            collect_fn_returns(&r.text, &mut env.extern_fns);
        }

        // 不动点传播：每轮扫描所有入边。
        let max_rounds = program.cells().count() + 1;
        for _ in 0..max_rounds {
            let before = env.declared.len();
            for src in program.cells() {
                for stmt in &src.body {
                    let Stmt::Jump(j) = stmt else { continue };
                    // `jump loop` 不跨 Cell 传递（§9.2 按位置刷新本 Cell 形参），
                    // 其余三种形态都是对 `targets` 里每个 Cell 的入边。
                    if j.kind == JumpKind::Loop {
                        continue;
                    }
                    for target in &j.targets {
                        // 目标 Cell 必须存在（不存在的目标由 E044/E040 在 check 层报）。
                        if program.cell(target).is_none() {
                            continue;
                        }
                        for arg in &j.args {
                            let key = arg.effective_key();
                            if key.is_empty() {
                                continue; // 无显式键名 → E040 由 check 层报
                            }
                            if env.declared_type(target, &key).is_some() {
                                continue; // 注解优先，不覆盖
                            }
                            if let Some(ty) = env.resolve_in(src, &arg.expr.text) {
                                env.declared.insert((target.clone(), key.clone()), ty);
                            }
                        }
                    }
                }
            }
            if env.declared.len() == before {
                break; // 收敛
            }
        }

        // 多包 receive `g1.W2.A`：类型来自组内派生实例 Cell 的 Ret 结构体里 `A` 字段
        // （S4 late binding——`g1` 在 body 内才创建，receive 却位于函数入口，
        //  所以这不是普通"入边实参"，而是"兄弟实例输出契约"）。
        for ((cell, key), ty) in multi_pkg_types(program, &env) {
            env.declared.entry((cell, key)).or_insert(ty);
        }
        env
    }
}

/// 解析多包 receive 键的类型。
///
/// 先建立 `group -> 派生目标 Cell` 映射（来自 `jump and <t> ... group <g>`），
/// 再对每个 `g.Wn.KEY` 键，去目标 Cell 的 `exit go with` 里找 `KEY` 的实参并推导类型。
fn multi_pkg_types(program: &Program, env: &TypeEnv) -> Vec<((Ident, Ident), String)> {
    let mut groups: HashMap<Ident, Ident> = HashMap::new();
    for c in program.cells() {
        for s in &c.body {
            if let Stmt::Jump(j) = s {
                if j.kind == JumpKind::And {
                    if let (Some(g), Some(t)) = (j.group.clone(), j.targets.first().cloned()) {
                        groups.insert(g, t);
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    for c in program.cells() {
        for key in &c.receive.keys {
            let Some(pkg) = key.pkg.as_deref() else { continue };
            let mut it = pkg.split('.');
            let Some(g) = it.next() else { continue };
            // 槽位段必须是 `W<n>`（§9.4 / D5 静态枚举）。
            if it.next().and_then(|w| w.strip_prefix('W')).is_none() {
                continue;
            }
            let Some(target) = groups.get(g) else { continue };
            let Some(tc) = program.cell(target) else { continue };
            if let Some(ty) = exit_key_type(tc, &key.name, env) {
                out.push(((c.name.clone(), key.name.clone()), ty));
            }
        }
    }
    out
}

/// Cell 的 `exit go with` 中，键 `key` 的实参类型。
fn exit_key_type(cell: &CellDef, key: &str, env: &TypeEnv) -> Option<String> {
    for s in &cell.body {
        if let Stmt::Exit { args, .. } = s {
            for a in args {
                if a.effective_key() == key {
                    return env.resolve_in(cell, &a.expr.text);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------- extern rust fn 签名（D2 规则 4）

/// `t` 是否是一次「函数调用」形状的表达式；是则返回被调函数名。
///
/// 只认 `<标识符>(..)` 且**参数表的闭括号就是文本末尾**：`f(a).len()`、`f(a) + 1`
/// 是更大的表达式，本规则不适用（那里类型由外层规则决定，不该由 `f` 的签名代言）。
fn call_target(t: &str) -> Option<&str> {
    let open = t.find('(')?;
    let name = t[..open].trim();
    if !rscel_parser::ast::is_plain_ident(name) {
        return None; // `Arc::new(..)` / `x.f(..)` / `f::<T>(..)` 之类
    }
    let rest = &t[open..];
    let mut depth = 0i32;
    for (idx, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return if idx + 1 == rest.len() { Some(name) } else { None };
                }
            }
            _ => {}
        }
    }
    None
}

/// Cell 文本里声明的 fn 返回类型（`Stmt::RawRust` 与 Cell 内 `extern rust{}` 块）。
///
/// 每次推导重扫而不缓存：Cell 体内的声明极少，而缓存要再造一张与 `declared`
/// 平行、还得随 `resolve_in` 的 `cell` 参数变化的表 —— 那张表本身才是错误来源。
fn cell_fn_returns(cell: &CellDef) -> HashMap<String, ExternRet> {
    let mut out = HashMap::new();
    for stmt in &cell.body {
        let text = match stmt {
            Stmt::RawRust(r) => &r.text,
            Stmt::ExternRustBlock { body, .. } => &body.text,
            _ => continue,
        };
        collect_fn_returns(text, &mut out);
    }
    out
}

/// 登记一条 fn 签名；同名而返回类型不同 → 记为歧义，此后该名字一律不采纳。
fn record_fn_return(map: &mut HashMap<String, ExternRet>, name: String, ret: String) {
    let clash = matches!(map.get(&name), Some(ExternRet::Known(existing)) if *existing != ret);
    if clash || !map.contains_key(&name) {
        map.insert(name, if clash { ExternRet::Ambiguous } else { ExternRet::Known(ret) });
    }
}

/// 词法级扫描 `fn <name>(<params>) [-> <ret>]` 声明并登记返回类型。
///
/// 为什么不用完整 Rust 解析：这里只需要「用户自己写在签名上的返回类型」这一条事实，
/// 而本项目零外部依赖（不能拉 `syn`）。**认不出就什么都不登记**，调用点回落到
/// "要求注解"——宁可多问一句，绝不猜类型（D2）。
///
/// 返回类型进推导表后即使偶有偏差也不会静默改语义：它同时被写进 `fn` 形参/字段声明
/// 与实参赋值两处，Rust 没有隐式数值转换，错了必然编译不过。
fn collect_fn_returns(text: &str, out: &mut HashMap<String, ExternRet>) {
    let chars: Vec<char> = scrub_rust_text(text).chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i + 1 < n {
        let is_kw = chars[i] == 'f'
            && chars[i + 1] == 'n'
            && (i == 0 || !is_ident_char(chars[i - 1]))
            && !chars.get(i + 2).is_some_and(|c| is_ident_char(*c));
        if !is_kw {
            i += 1;
            continue;
        }
        match parse_fn_sig(&chars, i + 2) {
            Some((name, ret, resume_at)) => {
                record_fn_return(out, name, ret);
                // 从参数表之后继续：函数体内的嵌套 `fn` 也是合法 Rust 声明。
                i = resume_at;
            }
            None => i += 2,
        }
    }
}

/// 从 `fn` 关键字之后解析签名，返回 `(名字, 返回类型, 参数表之后的下标)`。
fn parse_fn_sig(chars: &[char], from: usize) -> Option<(String, String, usize)> {
    let n = chars.len();
    let mut j = skip_ws(chars, from);
    let name_start = j;
    while j < n && is_ident_char(chars[j]) {
        j += 1;
    }
    if j == name_start {
        return None; // `fn(` / `fn (`：不是我们认识的声明形状
    }
    let name: String = chars[name_start..j].iter().collect();
    j = skip_ws(chars, j);
    // 带泛型参数列表的签名（`fn f<T>(..)`）不解析：`<` 的配平要懂 Rust 语法才靠得住。
    if j >= n || chars[j] == '<' || chars[j] != '(' {
        return None;
    }
    let after_params = close_paren(chars, j)?;
    let k = skip_ws(chars, after_params);
    let arrow = k + 1 < n && chars[k] == '-' && chars[k + 1] == '>';
    if !arrow {
        // 没有 `->` 就是返回 `()` —— 这是签名的事实，不是猜测。
        return Some((name, String::from("()"), after_params));
    }
    let ty_start = skip_ws(chars, k + 2);
    let ty_end = ret_type_end(chars, ty_start);
    let ty: String = chars[ty_start..ty_end].iter().collect::<String>().trim().to_string();
    if ty.is_empty() {
        return None; // `-> {` 之类的畸形写法
    }
    Some((name, ty, after_params))
}

/// 返回 `open` 处那对圆括号闭合之后的下标。
fn close_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, c) in chars.iter().enumerate().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// 返回类型文本的结束下标：到**括号深度 0** 上的 `{` 或 `;` 为止。
///
/// 深度判定只看圆/方括号：`-> Box<dyn Fn(i32) -> i64>` 里那个 `->` 的 `>` 不能把
/// 我们带偏，而返回类型里不可能出现未配平的 `{`。
fn ret_type_end(chars: &[char], from: usize) -> usize {
    let mut depth = 0i32;
    for (k, c) in chars.iter().enumerate().skip(from) {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' | ';' if depth == 0 => return k,
            _ => {}
        }
    }
    chars.len()
}

fn skip_ws(chars: &[char], mut j: usize) -> usize {
    while chars.get(j).is_some_and(|c| c.is_whitespace()) {
        j += 1;
    }
    j
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 把注释与字符串/字符字面量的**内容**抹成空格（换行与引号本身保留）。
///
/// 为什么必须先抹：扫描是词法级的，`// fn f() -> i64` 这样的注释、
/// `let tip = "fn g(x) -> i32";` 这样的字符串都会被当成声明抓走。
/// 注释里的类型是假的，采纳它等于猜类型 —— 违反 D2。
/// 已知不覆盖的形状（`r#".."#` 原始串里的 `fn` 文本、`'\u{..}'`）只会**漏登记**
/// 一条声明，代价是回落成"要求注解"，不会给出错误类型。
fn scrub_rust_text(text: &str) -> String {
    let cs: Vec<char> = text.chars().collect();
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '/' if cs.get(i + 1) == Some(&'/') => {
                while i < cs.len() && cs[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if cs.get(i + 1) == Some(&'*') => {
                let mut depth = 1usize;
                out.push_str("  ");
                i += 2;
                while i < cs.len() {
                    if cs[i] == '/' && cs.get(i + 1) == Some(&'*') {
                        depth += 1;
                        out.push_str("  ");
                        i += 2;
                    } else if cs[i] == '*' && cs.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        out.push_str("  ");
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        out.push(blank(cs[i]));
                        i += 1;
                    }
                }
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < cs.len() {
                    if cs[i] == '\\' {
                        out.push(' ');
                        i += 1;
                        if i < cs.len() {
                            out.push(blank(cs[i]));
                            i += 1;
                        }
                        continue;
                    }
                    if cs[i] == '"' {
                        out.push('"');
                        i += 1;
                        break;
                    }
                    out.push(blank(cs[i]));
                    i += 1;
                }
            }
            '\'' => {
                // 区分字符字面量与生命周期标注：`'x'` / `'\n'` 在两三格内闭合；
                // `&'a str` 的 `'` 后面是标识符而不是引号，必须**原样保留**，
                // 否则会把整段文本吃进"字面量"里，白白漏掉后面的真声明。
                let esc = cs.get(i + 1) == Some(&'\\');
                let is_char_lit =
                    if esc { cs.get(i + 3) == Some(&'\'') } else { cs.get(i + 2) == Some(&'\'') };
                out.push('\'');
                i += 1;
                if !is_char_lit {
                    continue;
                }
                let body = if esc { 2 } else { 1 };
                for _ in 0..body {
                    if i < cs.len() {
                        out.push(' ');
                        i += 1;
                    }
                }
                if i < cs.len() && cs[i] == '\'' {
                    out.push('\'');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------- 字面量判定

/// 去掉最外层成对括号。
fn strip_outer_parens(t: &str) -> Option<&str> {
    if !t.starts_with('(') || !t.ends_with(')') {
        return None;
    }
    let inner = &t[1..t.len() - 1];
    // 括号必须真正配对（`(a) + (b)` 的首尾括号不配对）。
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    if depth == 0 {
        Some(inner.trim())
    } else {
        None
    }
}

/// 字面量类型后缀：`5i64` → `i64`、`0.5f64` → `f64`。
///
/// 注意 `5i64` 在 Rust 里是**单个 token**，不能靠"非字母数字分界"切分
/// （那样 `5i64` 找不到边界），只能从已知数值类型名后缀匹配。
fn literal_suffix_type(t: &str) -> Option<String> {
    const NUMERIC: [&str; 14] = [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        "f32", "f64",
    ];
    // 长的后缀优先匹配（`i32` 与 `i8` 无歧义，但保持确定顺序更稳）。
    let mut best: Option<&str> = None;
    for ty in NUMERIC {
        if let Some(head) = t.strip_suffix(ty) {
            if head.is_empty() || !head.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '_') {
                continue;
            }
            if best.map_or(true, |b| b.len() < ty.len()) {
                best = Some(ty);
            }
        }
    }
    best.map(str::to_string)
}

/// 是否是一个可识别的 Rust 原始类型名。
fn is_rust_type_name(s: &str) -> bool {
    matches!(
        s,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "String"
            | "Str"
    )
}

/// 字面量 → 类型。含字符串/`bool`/整数/浮点。
fn literal_type(t: &str) -> Option<String> {
    if t == "true" || t == "false" {
        return Some("bool".into());
    }
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return Some("&'static str".into());
    }
    // 整数：允许 `0x` / `0b` / `0o` 前缀与 `_` 分隔。
    let digits = t.replace('_', "");
    let is_radix = digits.starts_with("0x") || digits.starts_with("0b") || digits.starts_with("0o");
    let body = if is_radix { &digits[2..] } else { &digits[..] };
    if !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || (is_radix && c.is_ascii_alphanumeric()))
        && body.chars().any(|c| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Some("i64".into());
    }
    // 浮点：`1.5` / `0.5e3`
    if t.contains('.') && t.chars().any(|c| c.is_ascii_digit()) {
        let core: String = t.chars().filter(|c| *c != '_').collect();
        let core = core.split('e').next().unwrap_or(&core).split('E').next().unwrap_or(&core);
        if core.chars().all(|c| c.is_ascii_digit() || c == '.') && core.contains('.') {
            return Some("f64".into());
        }
    }
    None
}

/// `f(<args>)` 形式里，`f` 全名等于 `name` 时返回参数列表原文。
fn wrapper_call<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    let rest = t.strip_prefix(name)?;
    let rest = rest.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner)
}

/// `vec![a, b, c]` → `["a", "b", "c"]`（顶层逗号切分，忽略括号内的逗号）。
fn slice_literal(t: &str) -> Option<Vec<String>> {
    let inner = t.strip_prefix("vec![").and_then(|r| r.strip_suffix(']'))?;
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in inner.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// 运算符表达式 → 类型。比较类 → bool，算术类 → i64。
fn operator_type(t: &str) -> Option<&'static str> {
    // 泛型类型引用（`Vec<i64>` / `HashMap<K, V>`）里的尖括号不是比较运算符。
    let is_type_ref = looks_like_type_ref(t);
    if !is_type_ref {
        for op in ["==", "!=", ">=", "<=", "&&", "||"] {
            if t.contains(op) {
                return Some("bool");
            }
        }
        if t.contains('<') || t.contains('>') {
            return Some("bool");
        }
    }
    if t.contains('*') || t.contains('/') || t.contains('%') {
        return Some("i64");
    }
    if t.contains('-') || t.contains('+') {
        return Some("i64");
    }
    None
}

/// 文本是否是一个**类型引用**而非值表达式：以 `>` 收尾，且紧邻 `<`/`>` 的 token 大写开头。
///
/// 用于把 `Vec<i64>`（类型）与 `a < b`（比较）区分开——两者都含尖括号。
///
/// 两个判据各挡一种误伤：
/// - **只看紧邻括号的 token**，不看整段前缀：`unknown_fn() as Vec<i64>` 的前缀含括号和
///   空格，按整段判定会被认成"比较表达式"而推出 `bool`。那是一个凭空猜出来的类型
///   （违反 D2），会把用户用来消歧的 `as` 变成更难懂的 rustc 类型错误。
/// - **必须以 `>` 收尾**：泛型引用的收尾就是 `>`，而比较表达式的右侧是个值
///   （`MAX > n`、`n < LIMIT` 的大写 token 是常量名，不是类型）。少了这条，
///   全大写常量在左的比较会从 `bool` 退化成"推不出"。
fn looks_like_type_ref(t: &str) -> bool {
    let Some(pos) = t.find(['<', '>']) else { return false };
    if !t.trim_end().ends_with('>') {
        return false;
    }
    let token: String = t[..pos]
        .trim_end()
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}
/// 找 Cell 内**带类型注解**的 let 声明：`let [mut] <name>: <T> = ...;` → `T`。
///
/// 扫描范围包含 `extern rust {}` 块与嵌套 RustBlock —— 那些位置的 let 同样是
/// 本 Cell 的局部绑定（§9.2 / D6），只扫 RawRust 会让注解被漏掉。
fn find_let_annotation(cell: &CellDef, name: &str) -> Option<String> {
        fn scan(text: &str, name: &str) -> Option<String> {
        for line in text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("let ") else { continue };
            let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if ident != name {
                continue;
            }
            let after = rest[ident.len()..].trim_start();
            let Some(ty_part) = after.strip_prefix(':') else { continue }; // 无注解
            let Some(eq) = ty_part.find('=') else { continue };
            if ty_part[eq + 1..].starts_with('=') {
                continue; // `: ==` 不是注解（比较运算的畸形写法）
            }
            let ty = ty_part[..eq].trim();
            if ty.is_empty() {
                continue;
            }
            return Some(ty.to_string());
        }
        None
    }

    for stmt in &cell.body {
        let text = match stmt {
            Stmt::RawRust(r) => &r.text,
            Stmt::ExternRustBlock { body, .. } => &body.text,
            _ => continue,
        };
        if let Some(ty) = scan(text, name) {
            return Some(ty);
        }
    }
    None
}


/// 在 cell.body 的 RawRust 透传文本里找 `let <name> = <rhs>;`，返回 rhs。
///
/// 覆盖三种声明形式：`let x = ..;`、`let mut x = ..;`、`let x: T = ..;`。
/// 名字边界必须校验——`let x` 不应误配到 `let xy = ...`。
fn find_let_rhs(cell: &CellDef, name: &str) -> Option<String> {
    let prefix = format!("let {name}");
    for stmt in &cell.body {
        let Stmt::RawRust(r) = stmt else { continue };
        for line in r.text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix(&prefix) else { continue };
            if !is_decl_boundary(rest) {
                continue; // `let xy = ..` 之类，不是我们要找的名字
            }
            let after = rest.trim_start();
            let after = after.strip_prefix("mut ").unwrap_or(after).trim_start();
            // 跳过可选的 `: <ty>` 注解段。
            let after = match after.strip_prefix(':') {
                Some(ac) => match ac.find('=') {
                    Some(eq) => &ac[eq..],
                    None => continue, // 只有类型注解、没有赋值 → 不是绑定
                },
                None => after,
            };
            // 跳过 `=`（排除 `==`）。
            let Some(rhs) = after.strip_prefix('=') else { continue };
            if rhs.starts_with('=') {
                continue;
            }
            let rhs = rhs.trim().trim_end_matches(';').trim();
            if !rhs.is_empty() {
                return Some(rhs.to_string());
            }
        }
    }
    None
}

/// `let <名字>` 之后是否紧跟一个能终结名字的字符（空白 / `:` / `=` / `mut`）。
fn is_decl_boundary(rest: &str) -> bool {
    rest.starts_with(' ')
        || rest.starts_with(':')
        || rest.starts_with('=')
        || rest.starts_with("mut ")
        || rest.starts_with("mut:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(name: &str, src: &str) -> CellDef {
        let out = rscel_parser::parse_program(src, 0);
        assert!(!out.has_errors(), "解析失败: {:#?}", out.diagnostics);
        out.program.cell(name).expect("cell 存在").clone()
    }

    #[test]
    fn annotation_wins_over_propagation() {
        let src = r#"
cell a;
receive(x: String);
exit go with();

cell b;
receive(y);
jump a go with(42 -> x);
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        assert_eq!(env.declared_type("a", "x"), Some("String"));
    }

    #[test]
    fn type_propagates_from_incoming_edge() {
        let src = r#"
cell a;
receive(x);
exit go with();

cell b;
receive(y);
jump a go with(42 -> x);
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        assert_eq!(env.declared_type("a", "x"), Some("i64"));
    }

    #[test]
    fn type_propagates_through_two_hops() {
        // a 的 x 无注解，值来自 b 的 y（也无注解），b 的 y 来自 c 的 42。
        let src = r#"
cell a;
receive(x);
exit go with();

cell b;
receive(y);
jump a go with(y -> x);
exit go with();

cell c;
receive();
jump b go with(42 -> y);
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        assert_eq!(env.declared_type("b", "y"), Some("i64"));
        assert_eq!(env.declared_type("a", "x"), Some("i64"));
    }

    #[test]
    fn literal_and_constructor_kinds() {
        let c = cell(
            "t",
            r#"
cell t;
receive();
let s: String = String::new();
let v = vec![1, 2, 3];
let f = 1.5;
let b = true;
exit go with();
"#,
        );
        let env = TypeEnv::default();
        assert_eq!(env.resolve_in(&c, "s").as_deref(), Some("String"));
        assert_eq!(env.resolve_in(&c, "v").as_deref(), Some("Vec<i64>"));
        assert_eq!(env.resolve_in(&c, "f").as_deref(), Some("f64"));
        assert_eq!(env.resolve_in(&c, "b").as_deref(), Some("bool"));
        assert_eq!(env.resolve_in(&c, "5i64").as_deref(), Some("i64"));
        assert_eq!(env.resolve_in(&c, "\"hi\"").as_deref(), Some("&'static str"));
        assert_eq!(env.resolve_in(&c, "1 + 2").as_deref(), Some("i64"));
        assert_eq!(env.resolve_in(&c, "1 < 2").as_deref(), Some("bool"));
    }

    #[test]
    fn unknown_expression_resolves_to_none() {
        // 推不出时必须返回 None（由生成阶段要求注解），不能猜。
        let c = cell("t", "cell t;\nreceive();\nexit go with();");
        let env = TypeEnv::default();
        assert_eq!(env.resolve_in(&c, "mystery_call()"), None);
        assert_eq!(env.resolve_in(&c, "Vec::new()"), None);
        assert_eq!(env.resolve_in(&c, "unknown_ident"), None);
    }

    #[test]
    fn arc_wrapper_infers_inner_type() {
        let src = r#"
cell a;
receive(h);
exit go with();

cell b;
receive();
jump a go with(Arc::new(7) -> h);
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        assert_eq!(env.declared_type("a", "h"), Some("Arc<i64>"));
    }

    // ------------------------------------------------------------ 规则 4：extern rust fn 签名

    /// 顶层 `extern rust {}` 里 `fn f(..) -> T` 的 `T` 是用户写下的权威事实，
    /// 采纳它不是"猜"；P0-2 的原始缺陷就是这个来源缺席，附录 A.6 样例只能
    /// 生成一串 `compile_error!`。
    #[test]
    fn extern_rust_fn_return_type_is_an_inference_source() {
        let src = r#"
extern rust {
    fn fast_mul(a: i64, b: i64) -> i64 { a.wrapping_mul(b) }
    fn greet(name: String) -> String { name }
};

cell main;
receive();
let p = fast_mul(6, 7);
jump sink go with(p -> n);
exit go with(greet("x".to_string()) -> s);

cell sink;
receive(n);
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        assert_eq!(env.declared_type("sink", "n"), Some("i64"), "let RHS → fn 签名两跳都要能推");
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(
            env.resolve_in(&main, "greet(\"x\".to_string())").as_deref(),
            Some("String")
        );
    }

    /// 参数表带生命周期标注时**不能**把整段文本当字符字面量吃掉：
    /// 漏登记只是"要求注解"，而吃掉后面的声明会给出错误的类型。
    #[test]
    fn lifetime_in_params_does_not_swallow_later_declarations() {
        let src = r#"
extern rust {
    fn takes_ref(s: &'static str) -> bool { !s.is_empty() }
    fn later() -> f64 { 0.5 }
};

cell main;
receive();
exit go with(later() -> v);
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(env.resolve_in(&main, "later()").as_deref(), Some("f64"));
        assert_eq!(env.resolve_in(&main, "takes_ref(\"a\")").as_deref(), Some("bool"));
    }

    #[test]
    fn unit_return_is_recorded_as_unit() {
        let src = r#"
extern rust {
    fn shout(msg: String) { println!("{msg}"); }
};

cell main;
receive();
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(env.resolve_in(&main, "shout(\"hi\".to_string())").as_deref(), Some("()"));
    }

    /// D2 的红线：同名 fn 携带不同返回类型时**不选一个**，回落到"要求注解"。
    #[test]
    fn ambiguous_fn_names_are_never_guessed() {
        let src = r#"
extern rust {
    fn dup() -> i64 { 0 }
};
extern rust {
    fn dup() -> String { String::new() }
};

cell main;
receive();
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(env.resolve_in(&main, "dup()"), None);
    }

    /// Cell 内 `extern rust{}` 是语句（D6），它声明的 fn 遮蔽同名顶层 item。
    #[test]
    fn cell_local_fn_shadows_global_one() {
        let src = r#"
extern rust {
    fn pick() -> i64 { 0 }
};

cell main;
receive();
extern rust { fn pick() -> String { String::new() } };
exit go with();

cell other;
receive();
exit go with(pick() -> v);
"#;
        let prog = rscel_parser::parse_program(src, 0).program;
        let env = TypeEnv::build(&prog);
        let main = prog.cell("main").unwrap();
        let other = prog.cell("other").unwrap();
        assert_eq!(env.resolve_in(main, "pick()").as_deref(), Some("String"), "本 Cell 的声明优先");
        assert_eq!(env.resolve_in(other, "pick()").as_deref(), Some("i64"), "看不见 main 的局部声明");
    }

    /// 注释里的 `fn` 不是声明：采纳它等于凭空造一个类型。
    #[test]
    fn commented_out_fn_is_not_adopted() {
        let src = r#"
extern rust {
    // fn ghost() -> i64 { 0 }
    let tip = "fn ghost2() -> String { .. }";
};

cell main;
receive();
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(env.resolve_in(&main, "ghost()"), None);
        assert_eq!(env.resolve_in(&main, "ghost2()"), None);
    }

    /// 显式 `as <类型>` 仍然压过 fn 签名（D2 第一规则：注解优先）。
    #[test]
    fn explicit_as_beats_fn_signature() {
        let src = r#"
extern rust {
    fn wide() -> i64 { 0 }
};

cell main;
receive();
exit go with();
"#;
        let env = TypeEnv::build(&rscel_parser::parse_program(src, 0).program);
        let main = rscel_parser::parse_program(src, 0).program.cell("main").unwrap().clone();
        assert_eq!(env.resolve_in(&main, "wide() as f64").as_deref(), Some("f64"));
    }

    /// 只有"整个表达式就是一次调用"才由签名代言；外层还有别的运算时不适用。
    #[test]
    fn only_a_bare_call_is_represented_by_its_signature() {
        assert_eq!(call_target("f(a, b)"), Some("f"));
        assert_eq!(call_target("f(a).len()"), None, "外层是方法调用，类型由 .len() 规则定");
        assert_eq!(call_target("f(a) + 1"), None);
        assert_eq!(call_target("mod::f(a)"), None, "路径调用不是裸标识符");
        assert_eq!(call_target("f::<T>(a)"), None, "turbofish 不解析签名泛型表");
        assert_eq!(call_target("(f(a))"), None, "外层括号由 strip_outer_parens 递归处理");
    }

    /// 尖括号在**类型引用**里不是小于号。判据只看紧邻括号的 token，
    /// 否则 `expr as Vec<i64>` 会被猜成 `bool` —— 凭空猜类型正是 D2 禁止的事。
    #[test]
    fn generic_type_after_as_is_not_read_as_a_comparison() {
        let c = cell("t", "cell t;\nreceive();\nexit go with();");
        let env = TypeEnv::default();
        assert_eq!(env.resolve_in(&c, "unknown_fn() as Vec<i64>"), None, "不得猜成 bool");
        assert_eq!(env.resolve_in(&c, "unknown_fn() as HashMap<String, i64>"), None);
        // 真比较仍然照常判定（紧邻 token 是小写或数字）。
        assert_eq!(env.resolve_in(&c, "1 < 2").as_deref(), Some("bool"));
        assert_eq!(env.resolve_in(&c, "a < b").as_deref(), Some("bool"));
        assert_eq!(env.resolve_in(&c, "x.len() > 0").as_deref(), Some("bool"));
        // 反向不误伤：右侧是全大写**常量**的比较，紧邻 token 大写但那不是类型引用
        // （判据要求文本以 `>` 收尾，只有泛型引用才是这种收尾）。
        assert_eq!(env.resolve_in(&c, "MAX > n").as_deref(), Some("bool"));
        assert_eq!(env.resolve_in(&c, "n < LIMIT").as_deref(), Some("bool"));
    }

    /// `looks_like_type_ref` 的两个判据单独钉住：`operator_type` 靠它决定要不要
    /// 把尖括号当成比较符。
    #[test]
    fn type_ref_detection_needs_both_the_token_and_the_closing_bracket() {
        assert!(looks_like_type_ref("Vec<i64>"));
        assert!(looks_like_type_ref("HashMap<String, i64>"));
        assert!(looks_like_type_ref("Box<dyn Fn(i32) -> i64>"));
        assert!(!looks_like_type_ref("MAX > n"), "右侧是常量，不是类型");
        assert!(!looks_like_type_ref("a < b"), "小写 token 是变量");
        assert!(!looks_like_type_ref("1 < 2"));
    }
}
