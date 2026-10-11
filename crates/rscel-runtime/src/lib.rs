//! # rscel-runtime —— RSupercell 运行时（批次 2 / 选型 S1）
//!
//! 零外部依赖的运行时模板（`Packet` + `Group` + `Semaphore` + 自研 executor）。
//!
//! ## 模块
//!
//! - [`packet`]：`VarHandle` + `Packet` + `From` impls（§13.1，B1 修法）；
//! - [`executor`]：多线程 worker 池 + `RawWaker` vtable（§13，S1）；
//! - [`semaphore`]：计数信号量 + async 友好（§6.2 / D4）；
//! - [`group`]：任务组（D4 增量回收 + B12 `spawn_owned` 修法）。
//!
//! ## 与 gen 路径的关系（S2）
//!
//! §1 选型 S2 决定 gen 主路径走类型化调用约定（不依赖本 crate 的 `Packet`）。本 crate
//! 仅供 [`--dyn-packets`] 后端（默认关闭）与运行时测试使用。

#![deny(unsafe_op_in_unsafe_fn)]
// Packet/VarHandle/TaskSlot/Waker 全部使用 unsafe，文档化安全保证。

pub mod executor;
pub mod group;
pub mod packet;
pub mod semaphore;

pub use executor::{Executor, JoinHandle};
pub use group::{Group, GroupInner};
pub use packet::{KeyIdx, Packet, VarHandle, VarKind};
pub use semaphore::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};