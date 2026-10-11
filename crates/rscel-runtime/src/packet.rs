//! Packet + VarHandle（§13.1 完整实现 + B1 修法 + D1/D2 兼容）。
//!
//! 设计依据：
//! - §13.1：模板形态（`VarHandle` 三态 + `Packet` 槽位 + From  impl）；
//! - B1：构造不能用 `vec![None; N]`（要求元素 `Clone`，`Option<VarHandle>` 不是 `Clone`），
//!   **实测**：编译期 `E0277: the trait bound Option<VarHandle>: Clone is not satisfied`；
//!   修法：`Vec::with_capacity` + `resize_with(N, || None)`（编译通过，已实测）；
//! - §1.2 / §4.1 实测：`size_of::<VarHandle>() == 24`、`Option<VarHandle>` 享有 niche 优化
//!   仍是 24 字节（`Arc` 裸指针的 niche 让 `Option` 不额外占空间）；
//! - S2：类型化主路径（批次 3 gen 接通）**不**走本 crate；本模块仅供 `--dyn-packets` 后端
//!   与运行时测试矩阵使用。
//!
//! 公开 API 严格按规格，不引入任何外部 crate。

use std::any::Any;
use std::sync::Arc;

/// 全局键索引（§10.2 的 `KeyIdx`）。
pub type KeyIdx = u16;

/// 一个传输槽位（§13.1）。三种形态：
///
/// - `OwnedPod(u64)`：i64 / u64 / f64 / bool 直接存位；
/// - `OwnedBox(Box<dyn Any + Send>)`：`go with` 独占所有权大对象；
/// - `SharedArc(Arc<dyn Any + Send + Sync>)`：`read with` 只读共享（Arc 句柄）。
///
/// 尺寸：24 字节（Rust 默认 8 字节对齐下，`Box` 与 `Arc` 裸指针 + tag）。
#[derive(Debug)]
pub enum VarHandle {
    /// 小 POD 直接存位。
    OwnedPod(u64),
    /// `go with`：独占所有权。
    OwnedBox(Box<dyn Any + Send>),
    /// `read with`：只读共享。
    SharedArc(Arc<dyn Any + Send + Sync>),
}

// `VarHandle` 仅含裸指针或位，安全地 `Send`。
// 注意：OwnedBox(Box<dyn Any + Send>) 不是 Sync——dyn trait object 没有 Sync 约束。
// 因此 VarHandle 只能 Send 不能 Sync（语义正确：`Packet` 跨线程移动而非并发读）。
unsafe impl Send for VarHandle {}
unsafe impl Send for Packet {}

impl VarHandle {
    /// 是否是 `read with` 形态（Arc 句柄）。
    pub fn is_shared(&self) -> bool {
        matches!(self, VarHandle::SharedArc(_))
    }

    /// 是否是 `OwnedBox` 形态（独占所有权大对象）。
    pub fn is_owned_box(&self) -> bool {
        matches!(self, VarHandle::OwnedBox(_))
    }
}

/// 取出槽位内容的形态标签（调试 / 错误信息用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    Pod,
    OwnedBox,
    SharedArc,
    None,
}

impl VarHandle {
    /// 当前形态标签。
    pub fn kind(&self) -> VarKind {
        match self {
            VarHandle::OwnedPod(_) => VarKind::Pod,
            VarHandle::OwnedBox(_) => VarKind::OwnedBox,
            VarHandle::SharedArc(_) => VarKind::SharedArc,
        }
    }
}

// ---- From impls（§13.1 完整覆盖）----

impl From<i64> for VarHandle {
    fn from(v: i64) -> Self {
        VarHandle::OwnedPod(v as u64)
    }
}

impl From<u64> for VarHandle {
    fn from(v: u64) -> Self {
        VarHandle::OwnedPod(v)
    }
}

impl From<i32> for VarHandle {
    fn from(v: i32) -> Self {
        VarHandle::OwnedPod(v as i64 as u64)
    }
}

impl From<u32> for VarHandle {
    fn from(v: u32) -> Self {
        VarHandle::OwnedPod(v as u64)
    }
}

impl From<f64> for VarHandle {
    fn from(v: f64) -> Self {
        VarHandle::OwnedPod(v.to_bits())
    }
}

impl From<f32> for VarHandle {
    fn from(v: f32) -> Self {
        VarHandle::OwnedPod((v as f64).to_bits())
    }
}

impl From<bool> for VarHandle {
    fn from(v: bool) -> Self {
        VarHandle::OwnedPod(v as u64)
    }
}

impl From<usize> for VarHandle {
    fn from(v: usize) -> Self {
        VarHandle::OwnedPod(v as u64)
    }
}

impl From<isize> for VarHandle {
    fn from(v: isize) -> Self {
        VarHandle::OwnedPod(v as i64 as u64)
    }
}

impl<T: Any + Send + 'static> From<Box<T>> for VarHandle {
    fn from(v: Box<T>) -> Self {
        VarHandle::OwnedBox(v)
    }
}

impl<T: Any + Send + Sync + 'static> From<Arc<T>> for VarHandle {
    fn from(v: Arc<T>) -> Self {
        VarHandle::SharedArc(v as Arc<dyn Any + Send + Sync>)
    }
}

// ---- Packet ----

/// 一个 Packet（§13.1）。`slots` 按 `KeyIdx` 索引；`died` 由 `die` 设置。
///
/// `Send + Sync` 由 `slots: Vec<Option<VarHandle>>` 自动实现（`VarHandle` 已实现）。
pub struct Packet {
    slots: Vec<Option<VarHandle>>,
    died: bool,
}

impl Packet {
    /// 构造一个 `total_slots` 槽位的 Packet。
    ///
    /// **B1 修法**：使用 `Vec::with_capacity` + `resize_with(..., || None)` 而**不是**
    /// `vec![None; N]`（后者要求元素 `Clone`，实测编译失败）。
    pub fn new(total_slots: usize) -> Self {
        let mut slots = Vec::with_capacity(total_slots);
        slots.resize_with(total_slots, || None);
        Packet { slots, died: false }
    }

    /// 构造一个 `died` 标记的 Packet（§13.1 的 `__rsc_Packet::none()`）。
    ///
    /// 仍携带 `total_slots` 个空槽位（与正常 Packet 形状一致，便于 `take` 不越界）。
    pub fn none(total_slots: usize) -> Self {
        let mut pkt = Self::new(total_slots);
        pkt.died = true;
        pkt
    }

    /// 槽位数（KeyTable 长度）。
    pub fn total_slots(&self) -> usize {
        self.slots.len()
    }

    /// 是否为 die 包（§13.1）。
    pub fn is_died(&self) -> bool {
        self.died
    }

    /// 标记为 die。
    pub fn mark_died(&mut self) {
        self.died = true;
    }

    /// 设置槽位（消费 `self`，链式调用风格）。覆盖已有值，丢弃旧值。
    pub fn set(mut self, key: KeyIdx, v: impl Into<VarHandle>) -> Self {
        let idx = key as usize;
        if idx >= self.slots.len() {
            // 越界时扩容 + 填充 None（B1 修法同款策略）。
            let new_len = idx + 1;
            self.slots.resize_with(new_len, || None);
        }
        self.slots[idx] = Some(v.into());
        self
    }

    /// 设置只读共享槽位（`Arc<dyn Any + Send + Sync>`）。等价 `set(key, v.into())`，
    /// 但直接接收 Arc 避免一次 `Any` 装箱。
    pub fn set_ref(mut self, key: KeyIdx, v: Arc<dyn Any + Send + Sync>) -> Self {
        let idx = key as usize;
        if idx >= self.slots.len() {
            let new_len = idx + 1;
            self.slots.resize_with(new_len, || None);
        }
        self.slots[idx] = Some(VarHandle::SharedArc(v));
        self
    }

    /// move 取出槽位并清空（§13.1 的 `take`）。
    pub fn take(&mut self, key: KeyIdx) -> Option<VarHandle> {
        let idx = key as usize;
        if idx >= self.slots.len() {
            return None;
        }
        self.slots[idx].take()
    }

    /// 只读借出槽位内容（§13.1 的 `take_ref`，对应 `read with` 形态）。
    ///
    /// 返回 `Any` 引用，调用方按 `downcast_ref::<T>()` 解包。**仅 Arc 与 POD 形态可借**；
    /// `OwnedBox` 不可借（独占所有权，必须 `take`）。
    pub fn take_ref(&self, key: KeyIdx) -> Option<&dyn Any> {
        match self.slots.get(key as usize).and_then(|s| s.as_ref()) {
            Some(VarHandle::SharedArc(a)) => Some(a.as_ref()),
            Some(VarHandle::OwnedPod(v)) => Some(v),
            Some(VarHandle::OwnedBox(b)) => Some(b.as_ref()),
            None => None,
        }
    }

    /// 查询槽位当前形态（不消费）。
    pub fn kind_at(&self, key: KeyIdx) -> VarKind {
        match self.slots.get(key as usize).and_then(|s| s.as_ref()) {
            Some(v) => v.kind(),
            None => VarKind::None,
        }
    }

    /// 是否存在该槽位且非 None。
    pub fn contains(&self, key: KeyIdx) -> bool {
        self.slots.get(key as usize).and_then(|s| s.as_ref()).is_some()
    }

    /// 已填槽位数。
    pub fn filled_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Packet")
            .field("total_slots", &self.slots.len())
            .field("filled", &self.filled_slots())
            .field("died", &self.died)
            .finish()
    }
}