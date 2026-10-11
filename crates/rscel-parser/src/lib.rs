//! # rscel-parser —— RSupercell 语言的词法分析器 / 语法分析器 / AST
//!
//! 规格依据：《RSupercell开发说明.md》§3（语法）、§7（词法）、§8（语法分析与 AST）、
//! §9（传输键名契约）、§15.1（诊断格式）、附录 B（错误码表）。
//!
//! 本 crate **零外部依赖**（只用 `std`），主入口永不 panic：
//!
//! ```
//! let out = rscel_parser::parse_program("cell main;\nreceive();\ndie;\n", 0);
//! assert!(!out.has_errors());
//! assert_eq!(out.program.cell("main").unwrap().body.len(), 1);
//! ```
//!
//! 模块划分：
//! - [`span`]：源位置（`Span` / `locate`）
//! - [`diag`]：诊断（`Diagnostic` / `Severity` / 错误码常量表）
//! - [`ast`]：AST（逐字段照抄规格 §8.1，含 3 处就地注释的扩展）
//! - [`lexer`]：按需驱动的扫描器（§7.3）
//! - [`parser`]：递归下降解析器（§8.2 / §8.3）与主入口

pub mod ast;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod span;

pub use ast::*;
pub use diag::{Diagnostic, Severity};
pub use parser::{parse_program, ParseOutcome, Parser};
pub use span::Span;
