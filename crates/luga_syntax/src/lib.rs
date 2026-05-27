pub mod lexer;
pub mod parser;
pub mod parsing;

pub use chumsky::span::{SimpleSpan, Span};
pub use parser::{
    Ast, BinOp, Conditional, EnumDecl, Expression, FunctionDecl, If, Intrinsic, Param, Spanned,
    Statement, StructDecl, StructInit, TopLevelItem, UnOp,
};
pub use parsing::{ParseFailure, parse_source, report_error};
