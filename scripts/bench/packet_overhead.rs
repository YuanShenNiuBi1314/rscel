// 选型基准：类型擦除 Packet 路径 vs 类型化调用路径的单点对比。
//
// 目的：为 docs/00-内存模型与选型报告.md §2.2 / §4.1 的 S2 决策提供可复现的实测数据。
// 场景：20 个全局键、2 个 i64 实参的 plain `jump`（对应附录 A.1 的 `jump fact go with(n, acc)`）。
// 零外部依赖，直接 rustc 编译：
//
//   rustc --edition 2021 -O scripts/bench/packet_overhead.rs -o target\bench\packet_overhead.exe
//   .\target\bench\packet_overhead.exe
//
// 2026-09-27 实测（rustc 1.97.1 -O）：A 类型擦除 88.76 ns，B 类型化 1.03 ns，86.4×。
// 数值随机器不同而不同，但量级差是结论性的：类型擦除的每次跳转固定成本是三位数量级中的一次
// 堆分配 + memset，去掉它才能守住 §17 的 ≥90% 门禁。

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

/// 全局键数量。取 20：真实程序的全局键规模（receive 键 + go with 键 + exit 键 + return 键）。
const KEY_TOTAL: u16 = 20;
const ITERS: u32 = 5_000_000;

// ---------------------------------------------------------------------------
// 路径 A：开发说明 §13.1 的类型擦除 Packet
// ---------------------------------------------------------------------------

pub enum VarHandle {
    OwnedPod(u64),
    OwnedBox(Box<dyn Any + Send>),
    SharedArc(Arc<dyn Any + Send + Sync>),
}

pub struct Packet {
    slots: Vec<Option<VarHandle>>,
}

impl Packet {
    pub fn new() -> Self {
        // 注意：文档原文写的是 `vec![None; N]`，它要求元素类型 Clone，
        // 而 VarHandle 含 Box/Arc 不是 Clone，编译报 E0277（报告 B1）。
        // 修法见下。
        let mut slots = Vec::with_capacity(KEY_TOTAL as usize);
        slots.resize_with(KEY_TOTAL as usize, || None);
        Self { slots }
    }

    #[inline]
    pub fn set(mut self, key: u16, v: impl Into<VarHandle>) -> Self {
        self.slots[key as usize] = Some(v.into());
        self
    }

    #[inline]
    pub fn take(&mut self, key: u16) -> Option<VarHandle> {
        self.slots[key as usize].take()
    }
}

impl From<i64> for VarHandle {
    #[inline]
    fn from(v: i64) -> Self {
        VarHandle::OwnedPod(v as u64)
    }
}

/// 目标 Cell：按契约从包里取两个键。
///
/// 这一步本身就在演示报告 B10：槽位里只有一个 `u64`，接收端必须运行时 `match`
/// 才能把 `OwnedPod` 还原成 `i64`；真实实现里还要 `downcast::<T>()` 走 Box 分支，
/// 而无类型注解的 `receive(x)` 根本无从决定 T。
#[inline(never)]
fn cell_a_erased(mut p: Packet) -> i64 {
    let a = match p.take(0).expect("contract: key 0 guaranteed") {
        VarHandle::OwnedPod(v) => v as i64,
        other => contract_violation(&other),
    };
    let b = match p.take(1).expect("contract: key 1 guaranteed") {
        VarHandle::OwnedPod(v) => v as i64,
        other => contract_violation(&other),
    };
    drop(p); // 未被 receive 选中的键随 Packet 析构
    a + b
}

#[cold]
#[inline(never)]
fn contract_violation(_: &VarHandle) -> i64 {
    // 契约保证走不到这里；等价于 downcast 失败时的 panic 分支。
    panic!("contract violation: expected OwnedPod")
}

// ---------------------------------------------------------------------------
// 路径 B：S2 静态类型化调用约定
// ---------------------------------------------------------------------------

/// 目标 Cell：参数即 receive 键，类型在编译期确定，零 Packet、零类型擦除。
#[inline(never)]
fn cell_a_typed(x: i64, y: i64) -> Option<i64> {
    // `Option` 是 exit/die 的类型化表达：None == die
    Some(x + y)
}

// ---------------------------------------------------------------------------
// 路径 C：read with 的 Arc 引用计数单价
// ---------------------------------------------------------------------------

/// 一次 `Arc::clone` + 一次 drop，即**单次 read with 转发**的最小单元。
///
/// 口径说明：`read with(x)` 生成 `Arc::clone(&x)`，接收方实例结束时句柄 drop。
/// 所以一次转发的真实成本 = 1 次 inc + 1 次 dec。下游三层转发就是 3 倍这个数，
/// **不是** 6 倍——早期版本把外层的一次 clone 也算进来，导致单价高估 3.7 倍。
#[inline(never)]
fn one_forward(a: Arc<Vec<u8>>) -> usize {
    a.len() // a 在此 drop → 引用计数 -1
}

/// 两次转发（对应"同一个句柄被连续 read with 两次"），用于验证线性关系。
#[inline(never)]
fn two_forwards(a: Arc<Vec<u8>>) -> usize {
    let b = Arc::clone(&a); // 2nd inc
    let n = b.len();
    drop(b); // 2nd dec
    let _ = a.len(); // a 在此 drop → 1st dec
    n
}

fn main() {
    println!("VarHandle           = {} bytes", std::mem::size_of::<VarHandle>());
    println!("Option<VarHandle>  = {} bytes (niche 优化，未额外占空间)", std::mem::size_of::<Option<VarHandle>>());
    println!("Packet slots       = {} keys = {} bytes", KEY_TOTAL, KEY_TOTAL as usize * std::mem::size_of::<Option<VarHandle>>());
    println!();

    // 预热，避免把首次调用的代码页/分支预测成本算进来
    for i in 0..100_000i64 {
        std::hint::black_box(cell_a_erased(Packet::new().set(0, i).set(1, i)));
        std::hint::black_box(cell_a_typed(i, i).unwrap());
    }

    let t = Instant::now();
    let mut acc = 0i64;
    for i in 0..ITERS as i64 {
        acc = std::hint::black_box(cell_a_erased(Packet::new().set(0, i).set(1, i)));
    }
    let erased = t.elapsed();

    let t = Instant::now();
    for i in 0..ITERS as i64 {
        acc = std::hint::black_box(cell_a_typed(i, i).unwrap());
    }
    let typed = t.elapsed();
    std::hint::black_box(acc);

    let big = Arc::new(vec![0u8; 4096]);
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..ITERS {
        n = std::hint::black_box(one_forward(Arc::clone(&big)));
    }
    let arc1 = t.elapsed();
    std::hint::black_box(n);

    let t = Instant::now();
    for _ in 0..ITERS {
        n = std::hint::black_box(two_forwards(Arc::clone(&big)));
    }
    let arc2 = t.elapsed();
    std::hint::black_box(n);

    let per = |d: std::time::Duration| d.as_secs_f64() * 1e9 / ITERS as f64;
    let (a, b, c1, c2) = (per(erased), per(typed), per(arc1), per(arc2));

    println!("每次 jump 调用（含包构造与析销）:");
    println!("  A 类型擦除 Packet : {a:>7.2} ns   (1 Vec 分配 + 480B memset + 2 次接收端匹配)");
    println!("  B 类型化直接传参  : {b:>7.2} ns   (按值传参，零分配)");
    println!("  固定开销 A - B    : {:>7.2} ns   (比值 {:.1}×)", a - b, a / b);
    println!();
    println!("read with 的 Arc 引用计数（每次转发 = 1 次 inc + 1 次 dec）:");
    println!("  C1 一次转发(1inc+1dec): {c1:>7.2} ns   ← read with 单价");
    println!("  C2 两次转发(2inc+2dec): {c2:>7.2} ns   （实测 {:.2}× C1，非严格 2×：差额是循环与传参的固定开销）", c2 / c1);
    println!("     三层转发(A.7 示例)  : {:>7.2} ns   = 3 × C1", c1 * 3.0);
    println!();

    // 结论表：不同业务耗时下，类型擦除路径能达到类型化路径的百分之多少
    println!("业务计算/次耗时 vs 类型化路径的性能保持率:");
    for biz in [10.0_f64, 50.0, 100.0, 1_000.0, 10_000.0] {
        let keep = (biz + b) / (biz + a) * 100.0;
        println!("  业务 {:>8.0} ns  ->  {:>6.1}%  {}", biz, keep, if keep >= 90.0 { "达标" } else { "不达标 (§17 要求 ≥90%)" });
    }
    println!();
    println!("本页所有数字同属一次运行，跨机器会有波动，量级差（A/B ≈ {:.0}×）才是结论。", a / b);
}
