//! # rscel-gen —— RSupercell 代码生成器（批次 3A：最小可验证端到端单元）
//!
//! 实现 S2 静态类型化 monomorphized 调用约定（[`docs/00-内存模型与选型报告.md` §2.2]）：
//! 每个 Cell 生成一个 `async fn cell_<name>(<typed_recv_args>) -> Option<Ret_<name>>`，
//! `plain jump` 直接 `return cell_<t>(<typed_args>).await`，零 Packet、零 `dyn Any`。
//!
//! 覆盖的语句集：`cell`、`receive`、`let` 透传、`jump loop`、`plain jump`、
//! `exit go with`、`die`、`out`、`jump recurse`/`jump and`/`jump sync`、`over`、
//! `concurrent_limit`、`read with`、`extern rust`（批次 A 已完成，见提交 2ff0f4b）。
//! 另外本 crate 同步产出 §12.9 的源映射表（[`srcmap`]），供 `rscel-cli` 做
//! §15.2 的 rustc 错误反映射。
//!
//! 接收类型来源按 **D2** 裁定实现（见 [`ty`]）：注解优先，否则从入边实参传播，
//! 仍推不出时发出 `compile_error!` 要求注解——绝不猜测类型。

use rscel_parser::Program;

pub mod codegen;
pub mod mangle;
pub mod rewrite;
pub mod runtime;
pub mod srcmap;
pub mod ty;

pub use codegen::GenOptions;

/// 代码生成结果：单个 .rs 文件的源码 + §12.9 的源映射表文本。
#[derive(Debug, Clone)]
pub struct GeneratedCode {
    /// 完整 Rust 源码（已嵌入 block_on 与 Ret 结构体）。
    pub rust_source: String,
    /// `.map.json` 文本（§12.9 形状：`version` / `file` / `cells` / `map[]`）。
    ///
    /// 为什么留 `Option`：形状由 §12.9 钉死（消费方 `rscel-cli` 按可缺省处理），
    /// 但本 crate 的**所有**正常路径都会填上它——`generate()` 走
    /// [`codegen::emit_program_mapped`]，映射表与源码一次算出。
    /// 类型推不出而只发 `compile_error!` 的那条路径同样返回 `Some`（`map` 为空数组），
    /// 因为「空映射」本身就是「生成物里没有任何 DSL 翻译」的准确陈述。
    pub source_map: Option<String>,
}

/// 主入口：对 `Program` 做代码生成（映射表的 `file` 字段用缺省名）。
///
/// 不做静态检查、不调用 rustc、不做任何 IO；纯函数式。返回的 `GeneratedCode.rust_source`
/// 直接交给 rustc 编译即可，`source_map` 落成同名的 `<out>.map.json`。
///
/// 需要 `.map.json` 里写真实生成文件名（`rscel-cli` 的场合）请用 [`generate_as`]。
pub fn generate(program: &Program) -> GeneratedCode {
    generate_as(program, srcmap::DEFAULT_FILE_NAME)
}

/// 与 [`generate`] 相同，但指定 `.map.json` 的 `file` 字段（= 落地的 `.rs` 文件名）。
///
/// **为什么要单独开这个入口**：§15.2 的反映射要靠 rustc 诊断里的 `file_name` 与
/// 映射表的 `file` 对得上；CLI 知道输出路径（`fl.rs`），库层不知道，
/// 所以由调用方把名字传进来，而不是在映射里留一个和磁盘上不一致的假名字。
pub fn generate_as(program: &Program, file_name: &str) -> GeneratedCode {
    generate_as_with(program, file_name, codegen::GenOptions::default())
}

/// 与 [`generate_as`] 相同，但可以开 §15.3 的 `--debug` 埋点（[`GenOptions::trace`]）。
///
/// **为什么单开一条而不给 `generate_as` 加一个 `bool`**：埋点改变生成物的字节，
/// 而性能门禁比的正是生成物 —— 把「release 一个字节都不许动」做成类型上可分辨的两条
/// 入口，比让每个调用方自己记得传 `false` 可靠得多
/// （`rscel-cli` 只有 `BuildMode::Debug` 这一条路径走这里）。
pub fn generate_as_with(
    program: &Program,
    file_name: &str,
    opts: codegen::GenOptions,
) -> GeneratedCode {
    let (rust_source, map) = codegen::emit_program_mapped(program, file_name, opts);
    GeneratedCode { rust_source, source_map: Some(map.to_json()) }
}
