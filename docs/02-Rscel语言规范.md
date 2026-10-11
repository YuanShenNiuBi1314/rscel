# RScell 语言规范（v0.3）

> **目标读者**：第一次接触 RScell 的工程师。读完本文，应能直接写出能跑通的 `.rscel` 文件并理解每一行背后的内存模型。
>
> **本文结构**：从动机 → 程序骨架 → Cell / receive → exit / die → jump → over / concurrent_limit → read with / Arc → extern rust → out → **所有权模型**（最重要） → entry / main → 类型注解 → 常见错误 → 完整示例 → 快速参考。
>
> **配套阅读**：
> - 内存模型与选型：`docs/00-内存模型与选型报告.md`
> - 选型决策：`docs/01-选型决策记录.md`（S1–S4）

---

## 目录

1. [RScell 是什么、解决什么问题](#1-rscell-是什么解决什么问题)
2. [第一个程序：5 分钟版](#2-第一个程序5-分钟版)
3. [程序骨架：编排层与计算层](#3-程序骨架编排层与计算层)
4. [Cell 与 receive：显式入口契约](#4-cell-与-receive显式入口契约)
5. [exit go with 与 die：唯一出口约束](#5-exit-go-with-与-die唯一出口约束)
6. [jump：拓扑化跳转](#6-jump拓扑化跳转)
7. [over + concurrent_limit：批量并行](#7-over--concurrent_limit批量并行)
8. [read with：只读共享](#8-read_with只读共享)
9. [extern rust：逃逸口](#9-extern-rust逃逸口)
10. [所有权模型：核心难点](#10-所有权模型核心难点)
11. [entry 与 main](#11-entry-与-main)
12. [类型注解与 D2 推导](#12-类型注解与-d2-推导)
13. [常见错误：18 条文档缺陷与裁定](#13-常见错误18-条文档缺陷与裁定)
14. [完整示例：附录 A 走通](#14-完整示例附录-a-走通)
15. [快速参考表](#15-快速参考表)

---

## 1. RScell 是什么、解决什么问题

### 1.1 一句话定位

**RScell 是一门编译到 Rust 的拓扑化业务编排 DSL**：以「Cell（格子）」为业务流程节点，以「函数」为计算实现。源码经前端编译、静态检查后翻译为 Rust 源码，再调用 `rustc` 编译为原生二进制。

### 1.2 为什么需要它

把「业务流程」与「业务计算」拆开，是大型业务系统（订单处理、支付链路、消息路由、审批流……）的常见做法：

- **业务流程**（编排层）：节点 + 跳转 + 消息传递——*结构化*、*可视化*、*可独立演化*。
- **业务计算**（计算层）：真正的算法、IO、类型定义——*性能敏感*、*生态丰富*。

传统做法：用 BPMN 写流程、用 Java/Python 写计算，再用胶水代码对接。问题：

1. **胶水代码本身就要测、要修**，相当于一个分布式系统里又多了一台机器。
2. **业务流程要可视化，但胶水代码没有结构**，断点调试都难。
3. **演进成本高**：加一个分支，要改 4 处（流程图 + 计算层 + 胶水 + 测试）。

RScell 的回答：**把编排层写成强类型 DSL**，编译到原生代码，**没有胶水**。

```
        编排层（RScell 源码）            计算层（Rust 源码）
┌────────────────────────────────┐    ┌────────────────────────────────┐
│  cell order_process;            │    │  extern rust {                │
│    receive(order -> ord);       │    │    fn calc_score(o: &Order)    │
│    jump validate when ...;      │ ─► │      -> Result<...> { ... }   │
│    jump route;                  │    │  }                             │
│    exit go with(...);           │    └────────────────────────────────┘
└────────────────────────────────┘
              │
              ▼
   ┌─────────────────────────┐
   │  rscel build order.rscel │ → cargo run → 可执行二进制
   └─────────────────────────┘
```

### 1.3 两层结构（这是 RScell 最重要的设计）

| 层 | 载体 | 职责 | 语法 |
| --- | --- | --- | --- |
| **编排层** | `cell` | 流程组织：接收参数、跳转、调用、传参、收口 | `cell` / `receive` / `jump` / `exit go with` / `die` / `out` |
| **计算层** | `extern rust` + 普通 Rust 函数 | 真正的业务计算、IO、类型定义 | `extern rust { ... }` 块、顶层 Rust item、Cell 内 `extern rust` 逃逸口 |

**核心约定**：

1. **Cell 保持薄**：Cell 的主要职责是「接收 → 跳转/调用/传参 → 出口」；重逻辑下沉到计算层。
2. **计算层使用 Rust 全部能力**：泛型、trait、crate 生态——零限制。
3. **Cell 内部是 Rust 透传区**，可自由调用 `let`、表达式、`if` 等控制流；但**块的顶层语句仍是 RScell 分派**（详见第 4 章）。

---

## 2. 第一个程序：5 分钟版

复制这段到 `hello.rscel`，然后 `rscel build hello.rscel -o hello.exe --release && ./hello.exe`：

```rscel
// hello.rscel — 最小可运行程序
cell main;
receive();
out("hello, RScell");
exit go with();
```

期望输出：

```
hello, RScell
```

### 2.1 这 3 行做了什么

- `cell main;` — 定义一个名为 `main` 的 Cell。
- `receive();` — 这个 Cell 入口不接收任何数据。
- `out("hello, RScell");` — debug 打印（生成 `println!`）。release 模式在后续批次会删除，目前保留。
- `exit go with();` — 收敛出口：本 Cell 产出空数据包。

### 2.2 编译产物等价于

```rust
fn cell_main() -> Option<RetMain> {
    println!("{}", "hello, RScell");
    return Some(RetMain {});
}
fn main() {
    let _ = __rsc_block_on(async move {
        let _ = cell_main();
    });
}
```

你不需要看 Rust 代码就能用 RScell——但当你想做 Rust 工程师擅长的优化（性能 / FFI / 异步生态）时，**生成代码就是合法 Rust，可以读懂、可以加注释、可以手写 extern rust 块插入**。

---

## 3. 程序骨架：编排层与计算层

### 3.1 一个最小程序的完整形状

```rscel
// === 顶层 Rust item（隐含 extern rust）===
use std::collections::HashMap;
struct Order { id: u64, amount: f64 }

// === 顶层 extern rust 块 ===
extern rust {
    fn fetch_order(id: u64) -> Order { /* ... */ }
    fn send_receipt(order: &Order, msg: &str) { /* ... */ }
}

// === 入口 Cell ===
cell main;
receive();
jump process go with(42 -> id);

// === 编排 Cell ===
cell process;
receive(id: u64);
let order = fetch_order(id);
jump validate when order.amount > 0;
out("invalid order");
die;

cell validate;
receive(order: Order);
send_receipt(&order, "validated");
exit go with(order -> o);

// === 也支持 entry 覆盖默认 main ===
// entry process;
```

### 3.2 顶层可出现的项（§3.3 文法）

按**文档顺序**任意出现：

1. `cell <name>;` — Cell 定义（多 Cell 任意排列）
2. `extern rust [read] { ... };` — 顶层 Rust 逃逸块（暴露类型 / 函数 / use）
3. `entry <cell_name>;` — 指定入口 Cell（可选；缺省取名为 `main` 的 Cell）
4. **顶层 Rust item**（`use` / `struct` / `enum` / `fn` / `trait` / `impl` / `const` / `static` / `type` / `mod`）——等价于隐含的 `extern rust{}`

### 3.3 顶层不能出现的项

- 任何 DSL 关键字（`jump` / `exit go with` / `die` / `out`）作为顶层语句
- `receive` 出现在顶层
- 裸整数、字符串、表达式

→ 顶层必须是「声明 / 入口 / Rust item」三类之一，否则 RSC-E032 报错。

---

## 4. Cell 与 receive：显式入口契约

### 4.1 Cell 是什么

**Cell 是一等函数**（first-class function）。它有一个入口（`receive`）、一段 body（编排 + 计算）、一个出口（`exit go with` / `die`）。

```rscel
cell <name>;
receive(<keys>) [as (<aliases>)];
<body statements>;
exit go with(<outputs>);   // 或 die;
```

### 4.2 receive：声明入口契约

```rscel
cell calc;
receive(a, b, c) as (x, y, z);
```

- `<keys>`：外部通过 `go with` / `read with` 送进来的**传输键名**。
- `as (<aliases>)`：本 Cell 内部使用的**本地别名**（如果省略，alias = 键名）。
- 数量必须相等（parser 层不强制，留给 C1 契约检查报 RSC-E042）。

### 4.3 类型注解

每个键可带 Rust 类型（**S2 类型化调用约定要求类型在编译期确定**）：

```rscel
cell fact;
receive(n: i64, acc: i64) as (cur, sum);
```

| 形式 | 语义 |
| --- | --- |
| `n` | 裸键名，alias = 键名 |
| `n: i64` | 带类型注解的键 |
| `as (cur)` | 别名 cur（与 `n` 等价的语义） |
| `as (cur: i64)` | 别名 + 类型注解（文档不一致，文档冲突，详见 §13） |

### 4.4 接收端的语义

- **选择性接收**：没写在 `receive` 里的键，即使上游传了，本 Cell 也看不见——`Packet` 销毁时丢弃。
- **未在 receive 的键不能访问**：报 RSC-E042。
- **let 不能遮蔽别名**：报 RSC-E070。
- **POD 类型（i64/u64/f64/bool/...）是 Copy**：可被多个 `go with` 反复使用（详见 §10）。

### 4.5 完整示例

```rscel
// receive 与 body 与 exit 三段式
cell order_handler;
receive(order_id: u64, retry_count: u32) as (oid, retry);
let order = fetch_order(oid);
if retry > 3 {
    out("too many retries for ", oid);
    die;
}
jump fulfill when order.amount > 0;
jump refund when order.amount <= 0;
die;  // unreachable，但 rscel-check C2 收敛检查负责，这里只是兜底

cell fulfill;
receive(o: Order);
send_receipt(&o, "fulfilling");
exit go with(o -> order);

cell refund;
receive(o: Order);
send_receipt(&o, "refunding");
exit go with(o -> order);
```

---

## 5. exit go with 与 die：唯一出口约束

### 5.1 exit go with：正常收敛出口

```rscel
exit go with(result -> k1, msg -> k2);
```

- 本 Cell 结束，**产出**一个返回包（包含 `k1, k2` 等键值对）。
- 键名可以省略——简写 `exit go with(result);` 等价于 `exit go with(result -> result);`。
- 表达式是 Rust 文本透传。

### 5.2 die：终止出口

```rscel
die;
```

- 本 Cell 销毁，**不产出任何返回包**——本 Cell 的本地变量、receive 别名全部 drop。
- `Packet` 销毁，未被消费的多包 receive 键缺失。

### 5.3 唯一出口约束（CF2 收敛检查）

**每个 Cell 的所有可达路径必须收敛到恰好一个出口**（`jump` / `exit go with` / `die`）。隐式"主路径坠落到 die"也算合法：

```rscel
cell branch;
receive(cond: bool);
jump a when cond;
jump b when !cond;
die;  // ← 显式兜底，rscel-check C2 验证所有路径已收敛
```

缺兜底的死路代码会被 RSC-E010 报错："分支可达但未收敛"。

### 5.4 out：debug 打印（不是出口）

```rscel
out("processing ", order_id);
out(x);           // 单参
out(a, b, c);     // 多参
```

- `out` 是 debug 打印（生成 `println!`），**不影响收敛检查**。
- release 模式在后续批次会删除 `out`，目前保留。

---

## 6. jump：拓扑化跳转

这是 RScell 最核心的语句。`jump` 有 **5 种形态**，每种语义不同：

| 形态 | 新建实例 | async 任务 | 跨边界 | Packet | 同步/出口 |
| --- | --- | --- | --- | --- | --- |
| `jump T` | ✅ | ❌（plain） | ✅ | ✅ | 同步出口 |
| `jump loop T` | ❌ | ❌ | ❌ | ❌ | 非出口 |
| `jump recurse T` | ✅ | ❌（默认内联 await） | ✅ | ✅ | 非出口 |
| `jump and T1, T2` | ✅ | ✅（真并行） | ✅ | ✅ | 非出口 |
| `jump sync G` | ❌ | ❌ | ❌ | ❌ | 非出口 |

### 6.1 形态 ①：`jump`（普通串行，互斥分支）

```rscel
cell entry;
receive(val: i64);
jump odd when val % 2 == 1 go with(val -> v);
jump even when val % 2 == 0 go with(val -> v);
die;
```

- **目标 Cell 收到 Packet 后启动新实例**；当前实例终止。
- **多条 jump 互斥**：命中一条则其余不执行（按 `when` 顺序判断）。
- `when <expr>` 是命中条件（透传 Rust 表达式）。
- `go with(<args>)` 按 key → 目标 receive 位置传参。
- `die` 是兜底分支出界。

**`when` 缺省 = 永远命中**（用 True）：无条件的 `jump` 100% 触发。

**未定的一条边界（`docs/05` P1-10，需拍板）**：上面那个样例能成立，是因为 `entry` 自己
**没有** `exit go with`（三条 jump 都不命中就 `die`）。如果本 Cell 的出口**带键**
（`exit go with(x -> v)`），而体内有一条**带 `when` 的 plain jump**，那么「命中即转移」
这条路径永远走不到那条 exit，也就没有任何值可以交回 `v`。当前实现不发明值：生成物在该
分支里构造空的 `Ret_<本 Cell> {}`，rustc 于是报 `E0063: missing field`（`rscel check`
现在不拦，反映射会把位置打回 `cell <名> @ 那一行`）。两条候选是「契约期直接拒」或
「给这条路径定义语义」，**不**用 `Ret::default()` 兜 —— 那等于把零值当成真算出来的结果
交给 `jump and` 的结果槽。

### 6.2 形态 ②：`jump loop`（原地迭代循环）

```rscel
cell fact;
receive(n: i64, acc: i64) as (cur, sum);
jump loop fact when cur > 1 go with(cur - 1 -> n, sum * cur -> acc);
exit go with(sum -> result);
```

- **目标必须为当前 Cell 自身**（rscel-check C1 验证）——否则报 RSC-E011。
- 原地复用实例：回到本 Cell 开头重新执行；不新建实例、不跨边界、无 Packet。
- 实参按位置刷新 `receive` 列表，**不使用键名**（§9.2 例外）——按位置对应，个数必须等于 receive 键数（否则 RSC-E041）。
- 不是出口：必须存在另一分支最终 `exit` / `die`，否则 RSC-E014（无限无出口循环）。

**关键细节**：jump loop 的循环变量（如 `cur - 1`）必须 `let`-rebind 出去，或直接来自入边实参。**不能写成 `jump loop fact when cur > 1 go with(cur - 1 -> n);`**——变量名解析问题。详见 §13。

### 6.3 形态 ③：`jump recurse`（真递归）

```rscel
cell fact;
receive(n: i64);
let mut sub_result: Option<i64> = None;
jump recurse fact when n > 1 go with(n - 1 -> n) return(res -> sub_result) group r1;
jump sync r1;
let result = n * sub_result.unwrap_or(1);
exit go with(result -> product);
```

- 生成全新子实例，父实例 `await` 子实例完成。
- `go with(...)` 同 plain jump。
- `return(<key> -> <alias>)`：子实例返回包中的 `key` 映射到父变量 `alias`。
- `group <name>`：该 jump 创建名为 `name` 的 group，子实例归入该 group。
- **D3 裁定**：默认内联 await，仅当下一条不是紧邻 `jump sync` 时才 spawn——见 §10 性能模型。
- 不是出口：后续继续执行，最终 `exit` / `die`。

### 6.4 形态 ④：`jump and`（并行派生）

```rscel
cell parent;
receive(g1.W1.A, g1.W2.A) as (a1, a2);
jump and worker1, worker2 when true go with(0 -> x) group g1;
jump sync g1;
out("worker1 returned ", a1, ", worker2 returned ", a2);
exit go with();

cell worker1;
receive(x: i64);
let result = x * 2;
exit go with(result -> A);

cell worker2;
receive(x: i64);
let result = x + 100;
exit go with(result -> A);
```

- 创建多个子实例，全部归入 group `<name>`。
- `jump sync <name>`：等待该 group 全部子实例完成。
- 子实例返回包自动命名为 `<group>.W1`、`<group>.W2`、...（按入组顺序）。
- **多源场景的 receive 必须用包限定**：`receive(g1.W1.A, g1.W2.A)`——否则 RSC-E043。
- 父侧需 D3 依赖检查（批 3C 实现）。

### 6.5 形态 ⑤：`jump sync`（等待 group 完成）

```rscel
jump and w1, w2 go with() group g;
jump sync g;  // 等待 g 完成
// 后续可访问 g.W1.<key>, g.W2.<key>
```

- 等待 group `<name>` 全部子实例完成。
- 必须有同名 group 在本 Cell 中通过 `jump and ... group <name>` 创建。
- 同一 group 只能 sync 一次（否则 RSC-E035）。

### 6.6 段顺序与语法

`jump` 语句按以下段序解析（固定）：

```
jump [loop|recurse|and] <targets>
     [when <expr>]
     [go with / read with / force go with / force read with (<args>)]
     [return(<key> -> <alias>, ...)]
     [over <collection> |> <op>(<closure>) ... ]
     [concurrent_limit = <expr>]
     [group <ident>] ;
```

---

## 7. over + concurrent_limit：批量并行

### 7.1 语法糖

```rscel
jump and worker over file_list
  |> filter(|f| f.size > 1024)
  |> map(|f| f.path)
  concurrent_limit = 32
  group g1;
jump sync g1;
```

- `over <coll>`：遍历集合，对每个元素派生一个子实例。
- `|> filter(...)` / `|> map(...)`：链式过滤/映射（闭包必须**纯函数**——C7 验证）。
- `concurrent_limit = <expr>`：信号量限流，最多同时 N 个实例。
- 等价展开（用户不可见，编译器自动做）：

```rscel
let sem = semaphore_new(32);
let g1 = group_new();
let iter = iter_new(file_list);
loop {
    let next_item = iter_next(iter);
    if next_item == none { break; }
    let keep = filter_closure(next_item);
    if !keep { continue; }
    let arg = map_closure(next_item);
    sem_acquire(sem);  // 排队
    spawn worker go with(arg) group g1 on_done { sem_release(sem); };
}
group_await(g1);
```

### 7.2 闭包纯函数规则（C7）

filter/map/flatmap/take/skip 的闭包体内：
- **禁止**：DSL 关键字（jump / exit go with / die / receive / go with / read with）。
- **禁止**：修改当前 Cell 的局部变量（只读捕获）。
- **允许**：读外部变量（只读）。

违反 → RSC-E080。

### 7.3 concurrent_limit 的依赖检查（D3）

带 `concurrent_limit` 的 `jump and`，子实例必须**完全独立**（互不依赖）。检查规则（**仅当带 concurrent_limit 才启用**）：

- 子 Cell 之间不能相互 jump / recurse。
- 子 Cell 之间不能共享 `read with` Arc。
- 子 Cell 的返回包不能被彼此读取。

例外：父通过 `read with` 下发同一只读 Arc 给全部子实例——不算子间依赖。

依赖检测到 → RSC-E050。

### 7.4 `jump` 的段序是固定的（写错顺序报 RSC-E033 并点名那一段）

一条 `jump` 的可选段必须按这个顺序写（规格 §11「jump 解析」）：

```
jump [loop|recurse|and] <目标列表> [when <expr>] [go with(..) | read with(..)
     | force go with(..) | force read with(..)] [return(..)] [over <coll> [|> ..]]
     [concurrent_limit=<expr>] [group <g>] ;
```

两个实际会踩的点：

1. **`read with` / `go with` 必须在 `over` 之前**。写成
   `jump and w over items read with(cfg) ...` 会得到 **RSC-E033**：消息点名错位的那一段
   （`` `read with` 这一段的位置不对 ``）并复述整条固定段序，`build` 就此停止、不产出 `.rs`。
   历史缺陷（已修，`docs/05` P1-1）：旧 parser 不报错，而是把 `read with(...)` 当成集合
   表达式吞掉，生成 `items read with(cfg).into_iter()` 这种非法 Rust，而 `rscel check`
   只给一条误导的 RSC-E042（"over 只能供给 1 个实参"，用户明明写了 `read with`）。
   正确写法：`jump and w read with(cfg) over items concurrent_limit=8 group g1;`
2. **`read with(x)` 按同名键绑定**：它填的是子 Cell 里**也叫 `x`** 的 receive 键，
   且那个键必须注解成 `Arc<..>`（否则 RSC-E037）。
   `read with(payload)` 配 `receive(data: Arc<String>)` 是不绑定的 → RSC-E042。

另外：`over <coll>` 会把集合 **move** 进组（§10.1），所以 `items.len()` 之类要在
`jump and ... over items` **之前**取。写在之后会得到 **RSC-E062**（引用发生在透传文本里，
例如 `let total = items.len();` / `println!("{}", items.len())` / `out(items.len())`）或
**RSC-E060**（引用发生在传输实参位，例如再写一次 `over items`、或 `go with(items -> k)`）；
`build` 就此停止。历史缺陷（已修，`docs/05` P1-2）：契约期**不拦**这种写法，
错误要到 rustc 的 E0382 才暴露，而那时报的是生成物的行号。

---

## 8. read with：只读共享

### 8.1 Arc 语义

```rscel
cell read_heavy_data;
receive(data: Vec<u8>);
let big_data = "some large read-only data";
jump recurse helper when true read with(big_data) group g1;
jump sync g1;
exit go with();

cell helper;
receive(big_data: Arc<Vec<u8>>);
jump recurse helper2 when true read with(big_data) group g2;
jump sync g2;
exit go with();

cell helper2;
receive(big_data: Arc<Vec<u8>>);
out("len = ", big_data.len());
exit go with();
```

- `read with(x)` 生成 `Arc::clone(&x)`，跨 Cell 共享**只读**句柄。
- 原始数据只存一份，每多一个接收方 refcount +1。
- 引用计数归零时原始数据释放——可能晚于子 Cell 完成（多层转发时）。

### 8.2 冻结（关键）

`read with` 分发的变量**冻结**——后续不能再 move（`go with`）也不能修改。

```rscel
cell bad;
receive(data: Vec<u8>);
jump other read with(data);  // data → Frozen
exit go with(data -> x);     // ✗ RSC-E061：Frozen 变量被 move
```

### 8.3 POD 类型禁 read with

小 POD（i64/u64/f64/bool）禁止 `read with`——直接传值就够了，Arc 引用计数得不偿失：

```rscel
cell bad;
receive(x: i64);
jump other read with(x);  // ✗ RSC-E063：小 POD 禁 read with
jump other force read with(x);  // ⚠ RSC-W060：用了 force 也建议改用 go with
```

---

## 9. extern rust：逃逸口

### 9.1 顶层块

```rscel
extern rust {
    use std::sync::Mutex;
    #[derive(Debug)]
    struct RawData { x: i64 }
    impl RawData {
        fn new(x: i64) -> Self { RawData { x } }
    }
    fn raw_calc(a: i64, b: i64) -> i64 { a * b + 1 }
};
```

- 块内是**完整 Rust 代码**，原样注入生成物顶部。
- DSL 不解析 `{}` 内部。
- 可声明 use / 类型 / 函数 / impl / 常量 / 静态变量等。
- 顶层裸写 Rust item（`use` / `struct` / `enum` / `fn` / `trait` / `impl` / `const` / `static` / `type` / `mod`）也是合法的——隐含的 `extern rust{}`。

### 9.2 Cell 内块

```rscel
cell hot_path;
receive(a: i64, b: i64) as (x, y);
extern rust {
    let temp = x * x + y * y;
    let result = temp / 2;
};
let out_val = result;
exit go with(out_val -> v);
```

- 块内变量在 block 内可见，**但不出 block**（Rust 块作用域）。Cell 内其他语句若要引用，需在块内完成所有计算，或重新声明。
- `read` 修饰（`extern rust read { ... }`）：块内禁止修改环境变量（仅读约束），由 C8 验证。

### 9.3 块内禁用 DSL 关键字

DSL 关键字（jump / receive / exit go with / die / out）**不能**作为块内语句。否则 RSC-E090。

---

## 10. 所有权模型：核心难点

这是 RScell 最容易踩坑的部分。

### 10.1 状态机

每个 Cell 内部**被跟踪的名字**有两种来源（`receive` 引入的别名、透传区里的 `let` 绑定），
四种状态：

```
        receive 键 / let 绑定
            │
            ▼
        ┌────────┐   go with / exit go with / over <coll>   ┌────────┐
        │ Active │ ───────────────────────────────────────► │ Moved  │（不可再用）
        └───┬────┘                                          └────────┘
            │ read with（仅 receive 别名）
            ▼
        ┌────────┐
        │ Frozen │（只读、禁止 move、禁止修改）
        └────────┘

        Copy ── 声明处已是 POD（§10.2）：不进 move 状态机，可反复使用
```

`over <coll>` 计入 move 一列，是因为 §7.1 定义它把集合交给并发组（每个元素派生一个实例），
父侧此后不能再读它——这正是 `bench/pipeline_and.rscel` 里 `items.len()` 必须写在
`jump and ... over items` **之前**的原因。

### 10.2 POD 是 Copy（POD 不走 move）

POD 类型（`i8/i16/i32/i64/i128/isize` / `u8/u16/.../usize` / `f32/f64` / `bool` / `char`）是 **Copy**。可被多个 `go with` / `read with` 反复使用，**不触发 E060**。

```rscel
cell a;
receive(val: i64);
jump b when true go with(val -> x);  // val is Copy → OK
jump c when true go with(val -> y);  // val is Copy → OK
exit go with();
```

非 POD（`String` / `Vec<T>` / `struct Foo` 等）每次 `go with` 是 move，第二次会报 E060。

`let` 绑定同一条规则，判据也在**声明处**：

```rscel
cell a;
receive();
let val: i64 = 7;              // 标了 POD 类型 → Copy，反复传值不报
let data: Vec<i64> = vec![1];  // 非 POD → 第二次 go with(data -> ..) 报 E060
let n = data.len();            // 未标注 → 按非 Copy 跟踪（与未标注的 receive 别名同口径）
exit go with();
```

**推不出 Copy 就照非 Copy 处理**，这是 D2「推不出就要求注解」在所有权检查里的表达：
想要 POD 豁免就写 `let n: i64 = ..`。

### 10.3 move 与 use

- **Active → Moved**：`go with(x)`、`exit go with(x)`、`over x`（`x` 是被跟踪的名字）。
- **使用已 Moved 的名字** → RSC-E060（传输实参位）/ RSC-E062（透传文本里）。
- **使用已 Frozen 别名** → RSC-E061（`go with` / `over` 这类 move 位置）。
- **在透传区（let/if/block 体）引用已 Moved/Frozen** → RSC-E062。
- **let 绑定名遮蔽 receive 别名** → RSC-E070。

引用判定的**分辨率口径**（写在这里，因为它决定什么能报、什么必须留给 rustc）：
透传文本是原样交给 Rust 的，本层没有类型信息，所以判定只做词法级识别，并刻意**宁可漏报
不误报**——误报会拦住一个合法程序，而漏报仍有 rustc 的 E0382 兜底。

| 处理 | 形状 | 为什么 |
| --- | --- | --- |
| 认作引用 | `let total = items.len();`、`println!("{}", items.len())`、`out(items.len())`、`when` 条件、`concurrent_limit`、`over .. \|> filter(\|f\| ..)` 的闭包体 | 花括号深度 0 的读取；括号/方括号**不**升深度 |
| 不认作引用 | 花括号**之内**的任何标识符（`if c { .. }`、`match` 分支、块内 `let`） | 模式绑定与读取在文本上同形，分不开就不猜 |
| 不认作引用 | `let` / `for` 的模式区、`\|f\| ..` 的形参、`fn(..)` 的形参列表、`.` 与 `::` 之后的段 | 那些位置是**绑定**或成员/路径名，不是变量读取 |
| 不认作引用 | 字符串、字符字面量、注释的内容 | `"items"` 与 `// items` 都不读值 |
| 恢复可用 | 同名 `let` 重新绑定（Rust 的遮蔽） | 之后引用的是新值；遮蔽 `receive` 别名另由 E070 报 |

### 10.4 jump loop 的所有权

`jump loop` 是原地循环——**状态回滚到入口快照**（同实例复用环境）。jump loop 的实参按位置刷新（写入 state = Active）。

但**`let` 变量**：spec §12.4 说"let 变量每轮重新绑定"。所以 `jump loop` 之前的 `let X = <expr>;` 必须每轮重新求值：

```rscel
cell fact;
receive(n: i64, acc: i64) as (cur, sum);
let next = cur - 1;        // ← 期望每轮重算
let next_acc = sum * cur;
jump loop fact when next > 0 go with(next, next_acc);
exit go with(next_acc -> result);
```

如果写成**不每轮重算**（即 `let` 留在 loop 外），会陷入死循环——因为 `next > 0` 永远是第一次算的值。**这是 spec A.1 的隐含 bug**（详见 §13 / B11）。

所有权检查器（C4）对回边的两条口径，写上面那段代码时值得知道：
- **回边不重走**：下一轮走的是同一条语句的文本，重走只会把同一条诊断报两次。
- **`when` 为假的 fallthrough 用这条语句之前的状态**：条件为假时 `go with(next, next_acc)`
  根本没有执行，所以 `exit go with(next_acc -> result)` 里的 `next_acc` 仍然可用（不报
  E060/E062）。上面的样例就是靠这条才不被误报。

### 10.5 jump and 与 Arc 的所有权

- 子实例独立拥有自己的 v_<id> 字段。
- `read with(x)` 生成 `Arc::clone(&x)`，**不**消耗 `x`——`x` 仍可继续 `read with` 给其他实例。
- `Arc` 出现在 `go with` 实参里 → RSC-E064（Arc 只能走 `read with` 转发）。
- **`over <coll>` 消耗集合**（§7.1 / §10.1）：`x` 之后再读它就是 use-after-move →
  E060（传输实参位）或 E062（透传文本）。冻结状态目前只登记在 `receive` 别名上，
  `let` 变量被 `read with` 分发后不冻结（`docs/05` P1-7 跟踪的已知保守面）。

---

## 11. entry 与 main

### 11.1 默认入口

```rscel
cell main;
receive();
<...>
```

名为 `main` 的 Cell 自动成为入口。

### 11.2 entry 覆盖

```rscel
entry start_flow;

cell start_flow;
receive();
<...>
```

`entry <cell_name>;` 覆盖默认入口，**可以没有 `cell main`**，但反过来——有 `cell main` 且声明 `entry X;` 也合法（X 优先）。

### 11.3 entry cell 的 receive

`entry` Cell 的 `receive` 键**按位置**从进程命令行实参取值（生成物里是
`std::env::args().skip(1)` 采集到的 `__rsc_argv`）。当前实现支持的类型与读法：

| 键类型 | 生成物的取法 | 缺参 / 读不到时 |
| --- | --- | --- |
| `String` | `__rsc_argv[i]` | `""` |
| `i8/i16/i32/i64/isize`、`u8/u16/u32/u64/usize`、`f32/f64` | `__rsc_argv[i].parse()` | `0`（**解析失败也是 `0`，不报错**） |
| `bool` | `__rsc_argv[i]` 等于 `"true"` 或 `"1"` | `false` |
| `char` | `__rsc_argv[i]` 的首字符 | `'\0'` |
| `Vec<String>` | **整个 `__rsc_argv`**（不消费位置：它后面的键仍从 `argv[1]` 起算） | 空 vec |
| 其余任何类型 | `Default::default()`，**命令行实参被丢弃** | 同左 |

所以 **entry cell 的接收参数类型必须 impl Default**——i64/u64/f64/bool/Vec<String> 等都满足；自定义 struct 需要 `#[derive(Default)]`。

⚠ 最后两行是**已知缺陷**（`docs/05-迭代待办.md` P1-5，2026-10-11 实测）：契约期零诊断，
静默降级只有跑起来才看得见。复现（同一份生成物，四种输入）：

```bash
printf 'entry main;\n\ncell main;\nreceive(rest: Vec<String>, n: i64, flag: bool);\nprintln!("rest={:?} n={} flag={}", rest, n, flag);\nexit go with();\n' > t.rscel
rscel check t.rscel                 # check OK: 1 cells, 0 errors
rscel build t.rscel -o t.exe && ./t.exe a b c d
# rest=["a","b","c","d"] n=0 flag=false   ← n 读的是 argv[1]="b"，解析失败静默变 0
```

在 P1-5 修掉之前：需要真实入参的键请只用上表前四行，且**不要**把 `Vec<String>` 与别的键混用。

---

## 12. 类型注解与 D2 推导

### 12.1 注解仍然是首选

`receive(...)` 里给参数**带 Rust 类型注解**是最稳的写法，也是文档所有样例的写法：

```rscel
cell fact;
receive(n: i64, acc: i64) as (cur, sum);  // ← : i64 必需
```

省略注解时，类型推导器（D2）按下面的**固定顺序**找类型，先命中先用：

| 顺序 | 来源 | 例子 |
| --- | --- | --- |
| 1 | `receive` 上的显式注解 | `receive(n: i64)` |
| 2 | 入边实参传播（迭代到收敛） | `jump a go with(42 -> x)` → `cell a` 的 `x` 是 `i64` |
| 3 | 表达式上的主动消歧（**只覆盖基本类型名**） | `exit go with(big() as f64 -> v)`；`as Vec<i64>` 不可用，见 12.3 |
| 4 | **调用点可见的 `extern rust` fn 签名** | `extern rust { fn fast_mul(a: i64, b: i64) -> i64 { .. } };` → `fast_mul(a, b)` 是 `i64` |
| 5 | 字面量 / 构造器 / 运算符 | `5i64`、`"s"`、`String::new()`、`Arc::new(x)`、`vec![1,2]`、`a + b`、`a == b` |
| 6 | Cell 内 `let` 绑定（递归回上面） | `let r = fast_mul(a, b);` → `r` 走规则 4 |

规则 4 的两个细则：

- **Cell 内的声明遮蔽顶层同名 item**（与 Rust 的词法作用域一致）：`extern rust {}` 写在
  Cell 里是语句（D6），它声明的 `fn` 只对该 Cell 可见。
- **只认「整个表达式就是一次裸调用」**：`f(a).len()`、`f(a) + 1`、`mod::f(a)`、`f::<T>(a)`
  都不由签名代言（外层有自己的规则，或者干脆推不出）。同名 fn 携带不同返回类型 → 判为
  歧义、一律不采纳。

### 12.2 推不出就要求注解，绝不猜

六档来源全部落空时，`rscel build` 只发出 `compile_error!` 并告诉你缺哪一个键的类型、
建议写成 `receive(k: T)`。**没有兜底猜测**：`i64` ↔ `u64` ↔ `f64` 猜错会静默改变程序
语义，比编译失败危险得多。

这条路径的生成物只有那几条 `compile_error!` 加一行空的 `fn main() {}`。补 main 不是为了
让编译通过（`compile_error!` 已保证必失败），而是因为一个没有 main 的 crate 会让 rustc
顺带报一条 `error[E0601]`：它跟用户的 DSL 毫无关系，却和真正的诊断同样醒目。顶层
`extern rust {}` 声明的 item 在这一行都还没有落地（提前返回发生在任何 item 生成之前），
所以不存在与用户自写的 `fn main` 重名冲突。

同一条红线也约束启发式本身：`a < b` 里的 `<` 是小于号，`Vec<i64>` 里的 `<` 是泛型
括号。判据有两条——**文本以 `>` 收尾**且**紧邻括号的那个 token 大写开头**，所以
`exit go with(compute() as Vec<i64> -> items)` 这类写法**不会**被猜成 `bool`，而是照本节
走"要求注解"；反过来，`MAX > n` 这种"全大写常量在右侧"的比较也不会被误认成类型引用。

### 12.3 `as <类型>` 只能用于基本类型

`<表达式> as <类型>` 会被**原样透传**进生成的 Rust，而 Rust 的 `as` 是数值/引用转换，
不是类型标注：`x as i64`、`s as String`… 合法；`v as Vec<i64>` 是非法 cast。
所以消歧形式只覆盖 i8/i16/i32/i64/i128/isize/u*/f32/f64/bool/char/String 这些名字。
**复杂类型的唯一正解是给键写注解**（`receive(items: Vec<i64>)`），
或让 `let items: Vec<i64> = ..;` 带上注解后按规则 6 递归推导。

---

## 13. 常见错误：18 条文档缺陷与裁定

实现过程中发现的两份规格文档（`RSupercell开发说明.md` 与 `语言设计文档.md`）互相矛盾或不严谨的 18 处缺陷。**这些是真实踩过的坑**，请避开：

| # | 缺陷 | 正确做法 |
| --- | --- | --- |
| **B1** | §13.1 用 `vec![None; N]` 初始化 Packet | 编译报错（`Option<VarHandle>: Clone` 不满足）。当前 rscel-runtime 用 `resize_with` 修复 |
| **B2** | §12.2 生成 `struct EnvC { v_a: _, v_b: _ }` | 字段不能是 `_` 占位。**rscel-gen 用普通局部变量 `v_<id>`**（D1 裁定） |
| **B3** | §13.1 类型擦除 + §3.4.1 允许无注解 receive | `Box<dyn Any>` 取出需要类型，无注解就编译不过。**rscel-gen 用 S2 类型化调用约定**（S2 选型） |
| **B4** | §12.5 + §13.1 `Group { tasks: Vec<JoinHandle> }` | `Vec<JoinHandle>` 仍累积所有句柄，10⁷ 文件爆内存。**rscel-runtime 增量回收**（D4 裁定） |
| **B5** | §8.1 AST 无 `Sync` 变体、§8.2 不认 `jump sync` | §11.2 引用 `JumpSync`。**rscel-parser 补 `Stmt::Sync`**（D7 裁定） |
| **B6** | §8.2 cell body 遇 `fn helper(){}` 即 break | cell 内 `fn` 嵌套是合法 Rust。**rscel-parser 仅遇 `cell` / `entry` 终止**（D6 修正） |
| **B7** | §A.2 `receive(g1.W1.A, g1.W2.A)` 时序不可能 | g1 在 body 内才创建。**rscel-gen 用 late binding**：包限定键在 `jump sync g1` 之后解包（**S4 选型**） |
| **B8** | §A.3 `over file_list`（运行期长度）vs 多包 receive 静态枚举 W1..Wn | 两者无法对齐。**rscel-check 报 RSC-E045**（D5 裁定） |
| **B9** | §5.4 自递归 `jump recurse` | `async fn` 自递归产生递归不透明类型，编译报 E0733。**rscel-gen 用 `Box::pin(...).await`**（自回边装箱） |
| **B10** | §13.1 `From<i64>` → `OwnedPod(v as u64)` | 类型擦除后无法解读 `u64` 是 i64 / f64 / bool。**rscel-gen 不走类型擦除**（S2 选型） |
| **B11** | §A.1 阶乘（recurse 版） | `fact(1)` 时不递归，`sub_result.unwrap()` panic。**样例须改为 `exit go with(sub.unwrap_or(1) * num)`** |
| **B12** | §12.5 调用 `g.spawn_owned(...)` | §13.1 `__rsc_Group` 无此方法，编译报 no method。**rscel-runtime 补 `spawn_owned`** |
| **B13** | §3.5.2 块内递归分派 | §8.1 `Stmt` 无处安放嵌套语句。**rscel-parser 补 `Stmt::RustBlock { block: RawBlock }`**（D8 裁定） |
| **B14** | §3.3 `receive(a, b)` vs 设计文档 `receive a, b`（无括号） | 两份文档语法不一致。**rscel-parser 接受两种形式** |
| **B15** | §3.4.1 `receive(n: i64) as (num: i64)` 别名带类型 | §3.3 文法中别名为纯 ident。**rscel-parser 接受并归到 `PacketKey::ty`** |
| **B16** | §1.2 `receive(order -> ord)` 行内别名 | §3.3 文法不支持。**rscel-parser 接受并归到 `aliases`** |
| **B17** | §3.5.4 顶层 Rust item 关键字集不全 | 缺 `pub` / `async` / `unsafe` / `extern "C"` / `macro_rules`。**rscel-parser 扩展识别**（只放宽识别，不放宽 DSL） |
| **B18** | §3.4.4 Cell 内 `extern rust{}` 与 §8.2 把 `extern rust` 列为 cell 终止符冲突 | 以 §3.3 文法为准：**Cell 内是语句，顶层才是 `Item::GlobalRust`**（D6 修正） |

更完整的发展历史与裁定见 `docs/00-内存模型与选型报告.md` §3。

---

## 14. 完整示例：附录 A 走通

### 14.1 A.1 阶乘（jump loop 版）— M1 验收

```rscel
cell main;
receive();
let five: i64 = 5;
jump fact go with(five -> n, 1i64 -> acc);

cell fact;
receive(n: i64, acc: i64) as (cur, sum);
let next = cur - 1;
let next_acc = sum * cur;
jump loop fact when next > 0 go with(next, next_acc);
out(next_acc);
exit go with(next_acc -> result);
```

编译运行：`rscel build factorial.rscel -o fl.exe --release && ./fl.exe` → 输出 **120** ✅

### 14.2 A.2 阶乘（jump recurse 版）— 修正版（B11）

```rscel
cell main;
receive();
jump fact go with(5i64 -> n);

cell fact;
receive(n: i64);
let mut sub: Option<i64> = None;
jump recurse fact when n > 1 go with((n - 1) -> n) return(res -> sub) group r1;
jump sync r1;
let result = n * sub.unwrap_or(1);
exit go with(result -> product);
```

### 14.3 A.3 一千万文件 + 并发限流（需要批次 3C 才能完整跑通）

```rscel
cell file_processor;
receive(path: String);
let hash = compute_hash(&path);
exit go with(hash -> h);

cell main;
receive();
let file_list = load_file_list();
jump and file_processor over file_list
  |> filter(|f| f.size > 1024)
  |> map(|f| f.path)
  concurrent_limit = 32
  group g1;
jump sync g1;
exit go with();
```

### 14.4 A.5 多分支互斥 + die

```rscel
entry start;

cell start;
receive(val: i64);
jump one when val == 1 go with(val -> n);
jump two when val == 2 go with(val -> n);
jump three when val == 3 go with(val -> n);
die;

cell one;
receive(n: i64);
out("one got ", n);
exit go with();

cell two;
receive(n: i64);
out("two got ", n);
exit go with();

cell three;
receive(n: i64);
out("three got ", n);
exit go with();
```

### 14.5 A.6 混合 DSL + extern rust（修正版）

```rscel
extern rust {
    fn fast_mul(a: i64, b: i64) -> i64 { a.wrapping_mul(b) }
};

cell main;
receive(a: i64, b: i64);
exit go with(fast_mul(a, b) -> product);
```

`product` 的类型 `i64` 来自 §12.1 规则 4（调用点 `fast_mul(..)` 命中顶层 `extern rust`
里那条签名的返回类型）。在 P0-2 之前这一格推不出类型，`rscel build` 只会发出
`compile_error!`——现在它产出可编译、可运行的程序（回归测试：
`rscel-gen/tests/gen_smoke.rs` 的 `appendix_a6_example_generates_runnable_code`）。

### 14.6 A.7 read with 多层转发（Arc 引用计数）

```rscel
cell a;
receive();
let big_data = "some large read-only data";
jump recurse b when true read with(big_data) group g1;
jump sync g1;
out("group g1 done");
exit go with();

cell b;
receive(big_data: Arc<Vec<u8>>);
jump recurse c when true read with(big_data) group g2;
jump sync g2;
exit go with();

cell c;
receive(big_data: Arc<Vec<u8>>);
out("len = ", big_data.len());
exit go with();
```

引用计数轨迹：`a → b → c`，各持有 Arc；全部 Cell 结束后原始字符串释放。

### 14.7 A.8 顶层 Rust item + entry 覆盖 + argv

```rscel
use std::collections::HashMap;

struct Order { id: u64, amount: f64 }
impl Order { fn total(&self) -> f64 { self.amount } }

entry start;

cell start;
receive(argv: Vec<String>);
let mut orders: HashMap<u64, Order> = HashMap::new();
jump report go with(orders -> map);

cell report;
receive(map: HashMap<u64, Order>);
let total: f64 = map.values().map(|o| o.total()).sum();
out(total);
exit go with();
```

（argv 入口**已经接上**：`Vec<String>` 键拿到全部命令行实参，见 §11.3 的表；
本例只声明了 `argv` 一个键，所以位置问题不会暴露。与其它键混用时的静默错位是
`docs/05` P1-5 跟踪的缺陷。）

---

## 15. 快速参考表

### 15.1 关键字

```
cell  receive  as  jump  loop  recurse  and  sync  group  when
go with  read with  exit go with  die  out  over  concurrent_limit
return  force  extern rust  entry
```

### 15.2 错误码速查（附录 B + §6 扩展）

| 码 | 触发 | 简述 |
| --- | --- | --- |
| RSC-E010 | C2 | 分支可达但未收敛 |
| RSC-E010 | C2 | **出口语句之后还有语句**（永不执行；§5.3 要求恰好一个出口，只报第一处） |
| RSC-E011 | C1 | jump loop 目标 ≠ 当前 Cell |
| RSC-E014 | C2 | 无限无出口循环 |
| RSC-E020 | reach | 缺入口（无 `cell main` 且无 `entry`） |
| RSC-E030 | lex | 未闭合字符串/括号/块注释 |
| RSC-E031 | lex | 孤立关键字 |
| RSC-E032 | lex | 顶层非法 item |
| RSC-E033 | parse | jump 语句解析失败（段序写错时点名错位的那一段并给出固定段序，§7.4） |
| RSC-E040 | C1 | 非标识符实参必须显式 `expr -> key` |
| RSC-E041 | C1 | jump loop 实参数 ≠ receive 键数 |
| RSC-E042 | C1 | receive 引用了未传入的键 |
| RSC-E043 | C1 | 多源并行 receive 必须 `包.变量` 限定 |
| RSC-E044 | C1 | 包名/键名不在子 Cell 输出契约 |
| RSC-E045 | C1 | over 派生数 ≠ 多包 receive 静态槽位数（D5/B8） |
| RSC-E050 | C3 | concurrent_limit 子依赖 |
| RSC-E060 | C4 | 变量已 move 重复使用（`go with` / `over` / 移出后再传） |
| RSC-E061 | C4 | 冻结变量被 move（`go with` / `over`） |
| RSC-E062 | C4 | move 后/冻结变量被透传区引用（受跟踪的名字含 `let` 绑定；判定口径见 §10.3） |
| RSC-E063 | C4 | 小 POD 禁 read with |
| RSC-E064 | C4 | Arc 句柄出现在 `go with` |
| RSC-E070 | C5 | let 遮蔽 receive 别名 |
| RSC-E080 | C7 | over 闭包非纯函数 |
| RSC-E090 | C8 | extern rust 块内有 DSL 关键字 |
| RSC-E034 | parser | cell 后缺 receive（D6） |
| RSC-E035 | C1 | jump sync group 未创建 / 已 sync 过 |
| RSC-E036 | parser | cell 名重复 |
| RSC-E036 | C1 | `jump recurse` 的 `return(k -> 别名)` 里那个别名不是 `let mut 别名: Option<T>` 声明（§6.3）——**与上面 parser 那一行同号**，见本表末尾的「已知缺陷：码值双头定义」 |
| RSC-E037 | parser | 多条 `entry` 声明 |
| RSC-E037 | C1 | `read with` 变量的接收类型不是 `Arc<T>`（§8.1，判定见 §A.3 那条）——**与上面 parser 那一行同号**，同上 |
| RSC-E038 | gen（**当前不打印**） | 接收/传参类型无法确定（需要注解）。规则本身由 gen 侧的 `compile_error!` **纯文本**实现（`crates/rscel-gen/src/codegen.rs:555` / `:571`，消息形如「cell `X` 的 receive 键 `k`（别名 `a`）无法确定类型；请写成 `receive(k: <类型>)」），输出里**没有** `RSC-E038` 这个码；`rscel-check` 的 `analysis.rs:25` 声明了同名常量但零引用。见 `docs/05`「已知文档与现实的偏差」 |
| RSC-E039 | C1 | `concurrent_limit` 的字面量无法转 `usize`（§7.1，含负数字面量） |
| RSC-E046 | C1 | 出口式 `jump`（plain）上挂了 `return(...)` / `group` / `over` / `concurrent_limit`（§6.1：转移式出口没有这些段落） |
| RSC-E047 | C1 | 同一个 recurse 的返回值既用 `return(k -> 别名)` 又用多包 `receive(g.W1.k)` 读取（§6.3：一个返回包两个消费者） |
| RSC-W001 | reach | cell 不可达（孤立） |
| RSC-W002 | C1 | over 集合运行期未知，多包 receive 取前 N |
| RSC-W060 | C4 | force read with 于小 POD（建议改 go with） |

> **已知缺陷：码值双头定义 + 补零口径不一致（2026-10-11 实测确认，跟踪在 `docs/05` P1-11）**。
> 上表里 `RSC-E036` / `RSC-E037` 各出现两次**不是笔误**：`rscel-parser` 的 `diag::code`
> （`crates/rscel-parser/src/diag.rs:183`/`:185`）与 `rscel-check` 的 `analysis::codes`
> （`crates/rscel-check/src/analysis.rs:23`/`:24`）给**两条完全不同的规则用了同一个数字**。
>
> **但"用户看到同一个字符串会混淆"这句本批复核时实测不成立**，真实的缺陷是另一条：
> 两侧的**渲染口径不同** —— parser 的 `Diagnostic::render` 用 `format!("{:03}", code)`
> 补零（`crates/rscel-parser/src/diag.rs` 的 render），check 走
> `rscel-cli` 的 `diag_print::format_diag`，那里是 `{code}` **不补零**。同一台机器、
> `cargo run -q -p rscel-cli -- check` 的实输出：
>
> | 程序 | 实际打印 | 该表登记的 |
> | --- | --- | --- |
> | `cell a; receive(); die; cell a; receive(); die;` | `RSC-E036: cell 名称重复定义：\`a\`…` | `RSC-E036`（一致） |
> | `receive(data: String); jump w read with(data);`（接收端不是 `Arc<T>`） | `RSC-E37: \`read with(data)\` 要求…` | `RSC-E037`（**少一位零**） |
> | `let mut r: i64 = 0i64; jump recurse … return(v -> r) group g;` | `RSC-E36: return 别名 \`r\` 必须…` | `RSC-E036`（**少一位零**） |
>
> → 于是**撞号在码值层（36/37 各两条规则），字符串层反而不撞**（`RSC-E36` ≠ `RSC-E036`）；
> 代价是全表 check 侧的码都少一位零，用户从终端里的 `RSC-E37` 搜不到本表的 `RSC-E037`。
> 这条比"同号不同义"更影响使用，因为它对**每一个** C1/C2/C4 诊断都成立。
>
> 同一批顺带查到 `RSC-E038` **没有任何 crate 发射**（只在 `analysis::codes:25` 声明后无人引用），
> 本表原来把它记成「gen 侧：接收/传参类型无法确定」属文档与现实不符 —— 已登记进
> `docs/05`「已知文档与现实的偏差」。**两条修法（统一补零 / 给撞号的规则重编号）都会改动
> 用户可见的码值或码串，属语言面决定，所以本批只登记、不改码**
> （推不出就报错、绝不猜的 D2 同样适用于码值：不悄悄重编号）。

### 15.3 Cell 形状速查

```
                ┌────────────────────────────────────────┐
                │ cell <name>;                            │
                │   receive(<keys> [as (<aliases>)];      │
                │   <body>                                │
                │   <terminal>: exit go with / die / jump │
                │ cell <name>;                            │
                └────────────────────────────────────────┘
```

### 15.4 jump 形态速查

| 形态 | 用途 | 关键约束 |
| --- | --- | --- |
| `jump T [when c] go with(args)` | 互斥分支 / 入口路由 | 多条互斥命中一条；落兜底 die |
| `jump loop T [when c] go with(args)` | 原地状态机循环 | T 必须是自身；位置刷新；按位置非按键 |
| `jump recurse T [when c] go with(args) [return(...)] group g` | 真递归 | 默认内联 await；不是出口 |
| `jump and T1, T2 [, ...] [when c] go with(args) group g` | 并行派生 | 真并行；非出口；可用 `concurrent_limit` |
| `jump sync g` | 等 group 完成 | 同一 group 一次；多源 receive 必须 `g.W*.key` |

### 15.5 所有权速查

| 操作 | 状态转移 |
| --- | --- |
| `receive x: i64` | `x` ← Active（Copy） |
| `receive x: String` | `x` ← Active（owned） |
| `go with(x)` `x` Active POD | `x` 仍 Active（Copy） |
| `go with(x)` `x` Active owned | `x` → Moved |
| `read with(x)` | `x` → Frozen |
| `exit go with(x)` | `x` → Moved |

### 15.6 与 Rust 语义对照

| RScell 概念 | Rust 对应 |
| --- | --- |
| `cell` | `async fn`（同步路径下也是 `fn`） |
| `receive(key1, key2)` | 函数参数 |
| `go with(arg1 -> key1)` | 实参 → 形参的键映射 |
| `exit go with(v -> k)` | `Ok(Ret_x { k: v })` |
| `die` | `return None` |
| `jump sync g` | `join_all(g).await` |
| `read with(big)` | `Arc::clone(&big)` |
| `over coll \|> filter(f) \|> map(g)` | `coll.into_iter().filter(f).map(g)` |
| `concurrent_limit = 32` | 信号量 `Semaphore::new(32)` |
| `force read with` | `Arc::clone`（覆盖自动类型策略） |
| `extern rust { ... }` | 直接嵌入 Rust 块 |

### 15.7 `--debug` 运行期事件速查（§15.3 埋点，2026-10-11 落地）

`rscel build --debug` 的产物会把下表这些事件打到 **stderr**（stdout 不受影响，用户程序
自己的输出照旧）。`@ 行:列` 是 **DSL** 的行列，与 §15.1 诊断、§12.9 映射表同源，
所以专用编辑器（`rscel edit`，构建模式选 `--debug`）能拿一条事件行直接高亮回源文件。

| 事件 | 含义 |
| --- | --- |
| `cell <名> spawn @ 行:列 recv=[键名表]` | 实例创建 + 它收到的 Packet（键名按 DSL 写法，多包键带 `g1.W1.` 前缀） |
| `cell <名> jump -> <目标> @ 行:列 send=[..]` | plain jump 的发送侧 |
| `cell <名> recurse -> <目标> @ 行:列 send=[..]` | `jump recurse` 内联路径（D3）的发送侧 |
| `cell <名> exit @ 行:列 send=[键名表]` | `exit go with` 交回结果，同时是本实例的销毁点 |
| `cell <名> implicit exit @ 行:列 send=[]` | body 以 `jump and` 收尾（fire-and-forget）时的兜底收敛 |
| `cell <名> die @ 行:列` | `die` 触发的 DSL 行号 |
| `group <组名> derive <调用方> -> <目标> limit={} items={} @ 行:列 send=[..]` | 整组派生规模（`limit`/`items` 是运行期真值） |
| `group <组名> permit cell=<目标> acquire` / `... release died=<是否 die>` | 并发令牌申请/归还，**每元素一对**；`died` 是"哪个实例死了"的唯一真相来源 |
| `group <组名> done cell=<目标> slots={} died_in_slots={} @ 行:列` | `jump sync` 处的组完成状态 |

三条口径（都是刻意的，不是没做完）：

1. **`extern rust {}` 块与顶层 Rust item 内部一行都不插**（§9 的逃逸口语义是"逐字透传"）。
2. **`jump loop` 不插**：§6.2 明文它是按键名契约的例外（原地刷新，不搬 Packet），
   而它在热循环里每轮一次，埋上就是每条循环一行 stderr。
3. **release 模式一行都不发**，且 release 的生成物必须与本功能存在之前**逐字相同**
   （含 `.map.json`）——它是 §17 性能门禁的被测对象，埋点渗进去会让分母漂移。
   这条由 `crates/rscel-gen/tests/gen_trace_test.rs` 与一次「HEAD 二进制 vs 本二进制各生成
   9 份 DSL 再 diff」的对照共同钉住（数据在 `docs/05` P2）。
4. `done` 的 `slots` / `died_in_slots` 只统计**被保留的槽位**（§6.4 / D5 让 `slots`
   之外的结果在生成侧就被丢弃）：没有 `g1.Wk` 接收方时它们恒为 `0`，即使真有实例 die。

无组名（没有 `jump sync` 可等）时组名格子给 `(anon)`。

### 15.8 `rscel build` 的增量缓存（§16.2，2026-10-11 落地）

`build` 默认在 `.rscel-cache/` 里缓存 **rustc 的产物**；同一条命令行再构建一次就跳过 rustc：

```
rscel build app.rscel -o app.exe --release              # 冷构建（写缓存）
rscel build app.rscel -o app.exe --release              # 命中：stderr 多一行「增量缓存命中，跳过 rustc（…）」
rscel build app.rscel -o app.exe --release --no-cache   # 既不读也不写，连缓存目录都不建
```

| 维度 | 规则 |
| --- | --- |
| 缓存根 | 从**源文件所在目录向上找**：最近的已存在 `.rscel-cache/` → 最近的含 `.git` 的祖先（项目根）→ 都没有就用源文件自己的目录 |
| 缓存键 | 整次 rustc 调用：生成物字节 + `.rs` 路径 + `-o` 路径 + 完整参数向量 + `rustc -vV` 全文。**逐字段**存成 `identity.bin`，命中要求它**逐字节相等**（目录名里的 FNV-1a 只是索引，碰撞最多白跑一次 `memcmp`）。条目分**两级**放：`.rscel-cache/{不含版本的身份}/{版本的索引}` —— 根那一层的名字里**没有**版本，所以"根不存在 ⇒ 这次调用从没编过"这个判定不必先花一次 `rustc -vV` 的进程启动（实测 52–67ms，`docs/05` P2-③b）；而版本全文仍然留在 `identity.bin` 里参与逐字节验证，**换工具链不可能吃旧产物**（由 `build_cache_test::the_version_is_verified_byte_for_byte_even_though_the_index_ignores_it` 钉住） |
| 命中时做什么 | 把条目里的 exe 字节写到 `-o`，并**重放同一次 rustc 的 stderr**（NDJSON → 走与冷构建同一个反映射函数，所以 RSC-E900/E901 与告警文本一字不差） |
| 命中时不做什么 | 只不刷新 `<out>.pdb`（同一条命令行的上一次冷构建把它留在同一路径上；真丢了就 `--no-cache` 编一次） |
| 不缓存什么 | 前端（parse / check / gen / `.rs` / `.map.json` / 反映射）**一行都不缓存**，所以诊断与映射表永远是本次实时的；构建失败（rustc 非零退出）也**不进缓存** |
| 失效 | 任何一项不同就是 miss：改 DSL 里影响生成物的东西、改 `--debug`/`--release`、换 `-o` 或 `.rs` 名、换 rustc（含 host triple）。换 rustc 是**另开一个叶子格子**（版本的索引在叶子名上），不是改写同一格，所以旧工具链的条目留在原地等着被 `prune` 淘汰。只改**注释**在 release 档照样命中（生成物没变）；在 `--debug` 档会 miss，因为 §15.3 的埋点把 DSL 的 `行:列` 印进了产物 |
| 条目上限 | 16 个**叶子条目**（按目录 mtime 淘汰最旧的），防止脚本反复换 `-o` 把盘占满；`prune` 同时回收 `v1` 遗留的扁平条目、过期的 `*.tmp-*`，以及被掏空的那层根目录 |

为什么规格 §16.2 字面那套「`<file>.ast.json` + 每 Cell 的 `<cell>.cfc.json` / `<cell>.gen.rs`」没照做，
以及为什么"按 Cell 拆 `mod`"与 §12 的实体展开冲突：实测数据在 `docs/05` P2 ③，
模块头 `crates/rscel-cli/src/build_cache.rs` 里逐条写明。

### 15.9 内嵌执行器 `__rsc_block_on` 的形状（S1 选型，2026-10-11 实测定形）

只有透传 Rust 里出现 `.await` 的 Cell 才生成 `async fn`（§12 / 批次 A 的规则）。同步 Cell
调 async Cell 时，调用点不发 `.await`（同步函数体里没有 async 上下文），而是用嵌入的
极简执行器驱动：

```rust
#[inline(never)]
fn __rsc_block_on<F: std::future::Future<Output = T>, T>(fut: F) -> T {
    // Waker = Arc<Sig{ Mutex<bool> + Condvar }>；不用忙等。
    let mut fut = std::pin::pin!(fut);      // ← 栈上 pin，不是 Box::pin
    loop {
        /* 先清标志再 poll，避免"poll 期间置位 → 无条件挂起"的丢失唤醒 */
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) { return v; }
        /* 否则在 Condvar 上等 wake */
    }
}
```

| 维度 | 规则 |
| --- | --- |
| 挂起 | `Condvar::wait`，**不忙等**；先清标志再 poll，避免丢失唤醒 |
| future 住在哪 | **调用线程的栈帧**（`std::pin::pin!`）。改成这样之前是 `Box::pin`，实测堆指针让 async 热循环每轮多 5 条穿过那个 Box 的存储，同窗单因素慢 **14%–17%**（`docs/06` 实验八：`GEN_nobox` 两窗 −16.6% / −14.2%，落地后反向复测 `GEN_box` 四窗 +18.1% / +18.9% / +18.9% / +17.2%） |
| `#[inline(never)]` | 保留。把执行器改成 `inline(always)` 实测打平（`GEN_alwaysbo` 两窗 0.981x / 1.021x，带内），而保住它让每个生成程序的栈帧只多一层 |
| async Cell 的展开 | **不做**，且是数据判掉的：`GEN_block`（body 展开进调用点的 `__rsc_block_on(async move { .. })`）与 `GEN_always`（只给 `async fn` 加内联提示）都打平或更慢。原因是 `jump loop` 原地循环让 Cell 的 `async fn` 边界整个运行**只被进入一次** |
| 边界（照实写） | 跨 await 存活的状态现在占调用线程的栈。执行器不递归，Cell 的 future 只装 DSL 局部，因此有界；用户在 `extern rust` 里**声明大数组并跨 await 持有**才会顶到这条线，而目前没有任何诊断提示那种写法 |
| 比值 | 本形状与同构手写（`bench/async_step_fn_hand.rs`，含同构执行器）两窗 **99.76% / 101.23%**，见 README 限制表「async 形状」那一行与 `docs/06` 实验八 |

---

## 附录：常见模式速查

### 模式：参数聚合

```rscel
cell aggregate;
receive(values: Vec<i64>) as (vs);
let sum = vs.iter().sum::<i64>();
exit go with(sum -> total);
```

### 模式：链式计算

```rscell
cell pipeline;
receive(x: i64);
let step1 = x * 2;
let step2 = step1 + 10;
jump next go with(step2 -> y);

cell next;
receive(y: i64);
out("final = ", y);
exit go with();
```

### 模式：缓存 + 重试

```rscel
cell fetch_with_retry;
receive(url: String, max_retries: u32) as (u, n);
let mut attempts: u32 = 0;
let mut result: Option<String> = None;
loop {
    attempts = attempts + 1;
    result = try_fetch(&u);
    jump loop fetch_with_retry when result.is_none() && attempts < n;
    jump return_result when result.is_some();
    die;
}
exit go with();

cell return_result;
receive();
// ...访问 result
exit go with();
```

（注意：`let mut`、状态回滚到入口、`let` re-bind 都要在 jump loop 内每次重做；上面 `attempts` 与 `result` 也会被回滚——这里用 `cell` 的状态机模拟不严谨。**严格实现**：把可变状态显式存到 receive 参数的引用类型里，或用 `cell return_result` 接收 result。**示例仅展示语法。**）

### 模式：超时控制

```rscel
cell with_timeout;
receive(work_ms: u64, deadline_ms: u64);
let started = now();
jump worker read with(started) when work_ms > 0;
die;
```

（具体并发控制留待批次 3C 的 over + concurrent_limit。）

---

**阅读完毕**：你应该能写出任何 1-10 个 Cell 的 .rscel 程序，理解其生成的 Rust 等价代码，并诊断 RSC-E0xx 系列错误。下一步：实际动手 `rscel build` 一个你关心的例子，把 §14 的附录 A 全部跑一遍。
