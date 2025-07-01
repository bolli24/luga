use std::fmt::Display;

use crate::lexer::Token;
use chumsky::{input::ValueInput, prelude::*};

#[derive(Clone, Debug)]
pub struct Program {
    pub statements: Vec<Spanned<Statement>>,
}

#[derive(Debug, Clone)]
pub struct Spanned<T: std::fmt::Debug + Clone>(pub T, pub SimpleSpan);

#[derive(Clone, Debug)]
pub enum Statement {
    Expression(Spanned<Expression>),
    Binding(String, Spanned<Expression>),
    Assignment(String, Spanned<Expression>),
    Return(Spanned<Expression>),
    Block(Vec<Spanned<Statement>>),
    If(
        Box<Spanned<Expression>>,
        Vec<Spanned<Statement>>,
        Vec<Spanned<Statement>>,
    ),
    FunctionDecl(FunctionDecl),
}

#[derive(Clone, Debug)]
pub struct FunctionDecl {
    pub name: String,
    pub r#extern: bool,
    pub params: Vec<Param>,
    pub body: Vec<Spanned<Statement>>,
    pub return_type: TypeName,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Spanned<String>,
    pub type_name: Spanned<String>,
}

type TypeName = String;

#[derive(Clone, Debug)]
pub enum Expression {
    Identifier(String),
    Float(f64),
    Integer(i64),
    String(String),
    Bool(bool),

    Intrinsic(Intrinsic),
    FunctionCall(Spanned<String>, Vec<Spanned<Expression>>),
    Binary(BinOp, Box<Spanned<Expression>>, Box<Spanned<Expression>>),
}

#[derive(Clone, Copy, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Equal,
}

impl Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Equal => write!(f, "=="),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Intrinsic {
    Print(Vec<Spanned<Expression>>),
    Assert(Vec<Spanned<Expression>>),
    Exit(Vec<Spanned<Expression>>),
}

impl Intrinsic {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Print(_) => "@print",
            Self::Assert(_) => "@assert",
            Self::Exit(_) => "@exit",
        }
    }
}

const KEY_WORDS: [&str; 7] = ["let", "return", "if", "else", "true", "false", "fn"];

pub fn luga_parser<'a, I>() -> impl Parser<'a, I, Program, extra::Err<Rich<'a, Token<'a>>>> + Clone
where
    I: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan>,
{
    let identifier = select! {
        Token::Id(name) => name
    }
    .filter(|name| !KEY_WORDS.iter().any(|key_word| key_word == name))
    .labelled("identifier");

    let identifier_spanned = identifier.map_with(|id, extra| Spanned(id.to_owned(), extra.span()));

    let float = select! {Token::Float(n) => n}.try_map(|n, span| {
        n.parse::<f64>()
            .map_err(|_| Rich::custom(span, "Invalid float literal."))
    });

    let integer = select! {Token::Integer(n) => n}.try_map(|n, span| {
        n.parse::<i64>()
            .map_err(|_| Rich::custom(span, "Invalid integer literal."))
    });

    let string = select! {Token::Quoted(s) => s};

    let bool = select! {
        Token::Id("true") => Expression::Bool(true),
        Token::Id("false") => Expression::Bool(false)
    };

    let bin_op = select! {
        Token::Plus => BinOp::Add,
        Token::Minus => BinOp::Sub,
        Token::Star => BinOp::Mul,
        Token::Slash => BinOp::Div,
        Token::EqualEqual => BinOp::Equal,
    };

    let mut expression = Recursive::declare();

    let atom = choice((
        identifier.map(|name| Expression::Identifier(name.to_owned())),
        float.map(Expression::Float),
        integer.map(Expression::Integer),
        string.map(Expression::String),
        bool,
        expression
            .clone()
            .delimited_by(just(Token::ParenOpen), just(Token::ParenClose))
            .map(|expr: Spanned<Expression>| expr.0),
    ))
    .map_with(|expr: Expression, extra| -> Spanned<Expression> { Spanned(expr, extra.span()) });

    let factor = atom.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Mul | BinOp::Div))
            .then(atom.clone())
            .repeated(),
        |a, (op, b), extra| {
            Spanned(
                Expression::Binary(op, Box::new(a), Box::new(b)),
                extra.span(),
            )
        },
    );

    let term = factor.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Add | BinOp::Sub))
            .then(factor.clone())
            .repeated(),
        |a, (op, b), extra| {
            Spanned(
                Expression::Binary(op, Box::new(a), Box::new(b)),
                extra.span(),
            )
        },
    );

    let equality = term.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Equal))
            .then(term.clone())
            .repeated(),
        |a, (op, b), extra| {
            Spanned(
                Expression::Binary(op, Box::new(a), Box::new(b)),
                extra.span(),
            )
        },
    );

    let mut statement = Recursive::declare();

    let block = statement
        .clone()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::BraceOpen), just(Token::BraceClose));

    let args = expression
        .clone()
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let decl_params = (identifier_spanned
        .then_ignore(just(Token::Colon))
        .then(identifier_spanned)
        .map(|(name, type_name)| Param {
            name,
            type_name,
        }))
    .separated_by(just(Token::Comma))
    .collect()
    .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let function_decl = just(Token::Id("fn"))
        .ignore_then(identifier)
        .then(decl_params)
        .then(block.clone())
        .map(|((name, params), body)| FunctionDecl {
            name: name.to_owned(),
            r#extern: false,
            params,
            body,
            return_type: "void".to_owned(),
        });

    let function_call = identifier_spanned
        .then(args.clone())
        .map_with(|(name, args), extra| {
            Spanned(Expression::FunctionCall(name, args), extra.span())
        });

    let intrinsic = just(Token::At)
        .ignore_then(identifier)
        .then(args)
        .try_map(|(id, call), span| match id {
            "print" => Ok(Intrinsic::Print(call)),
            "assert" => Ok(Intrinsic::Assert(call)),
            "exit" => Ok(Intrinsic::Exit(call)),
            _ => Err(Rich::custom(span, "Unknown intrinsic.")),
        })
        .map_with(|intrinsic, extra| Spanned(Expression::Intrinsic(intrinsic), extra.span()));

    let else_block = just(Token::Id("else")).ignore_then(block.clone());

    let if_statement = just(Token::Id("if"))
        .ignore_then(
            expression
                .clone()
                .delimited_by(just(Token::ParenOpen), just(Token::ParenClose)),
        )
        .then(block.clone())
        .then(else_block.or_not())
        .map_with(|((condition, block), else_block), extra| {
            Spanned(
                Statement::If(Box::new(condition), block, else_block.unwrap_or(Vec::new())),
                extra.span(),
            )
        });

    expression.define(choice((intrinsic, function_call, equality)).labelled("expression"));

    let binding = just(Token::Id("let"))
        .ignore_then(identifier)
        .then_ignore(just(Token::Equal))
        .then(expression.clone())
        .map_with(|(ident, expr), extra| -> Spanned<Statement> {
            Spanned(Statement::Binding(ident.to_owned(), expr), extra.span())
        });

    let assignment = identifier
        .then_ignore(just(Token::Equal))
        .then(expression.clone())
        .map_with(|(ident, expr), extra| -> Spanned<Statement> {
            Spanned(Statement::Assignment(ident.to_owned(), expr), extra.span())
        });

    let r#return = just(Token::Id("return"))
        .ignore_then(expression.clone())
        .map_with(|expr: Spanned<Expression>, extra| -> Spanned<Statement> {
            Spanned(Statement::Return(expr), extra.span())
        });

    statement.define(
        choice((
            r#return,
            binding,
            assignment,
            expression.map_with(|expr: Spanned<Expression>, extra| {
                Spanned(Statement::Expression(expr), extra.span())
            }),
        ))
        .then_ignore(just(Token::Semicolon))
        .or(choice((
            if_statement,
            block.map_with(|block, extra| Spanned(Statement::Block(block), extra.span())),
            function_decl
                .map_with(|decl, extra| Spanned(Statement::FunctionDecl(decl), extra.span())),
        ))),
    );

    let program = statement.repeated().collect::<Vec<_>>();

    program.map(|statements| Program { statements })
}
