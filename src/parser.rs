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
}

#[derive(Clone, Debug)]
pub enum Expression {
    Identifier(String),
    Float(f64),
    Integer(i64),
    String(String),
    Bool(bool),

    Intrinsic(Intrinsic),
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
            BinOp::Add => write!(f, "+"),
            BinOp::Sub => write!(f, "-"),
            BinOp::Mul => write!(f, "*"),
            BinOp::Div => write!(f, "/"),
            BinOp::Equal => write!(f, "=="),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Intrinsic {
    Print(Vec<Spanned<Expression>>),
}

impl Intrinsic {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Print(_) => "@print",
        }
    }
}

const KEY_WORDS: [&str; 6] = ["let", "return", "if", "else", "true", "false"];

pub fn luga_parser<'a, I>() -> impl Parser<'a, I, Program, extra::Err<Rich<'a, Token<'a>>>> + Clone
where
    I: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan>,
{
    let identifier = select! {
        Token::Id(name) => name
    }
    .filter(|name| !KEY_WORDS.iter().any(|key_word| key_word == name))
    .labelled("identifier");

    let float = select! {Token::Float(n) => n}.try_map(|n, span| {
        n.parse::<f64>()
            .map_err(|_| Rich::custom(span, "Invalid float literal."))
    });

    let integer = select! {Token::Integer(n) => n}.try_map(|n, span| {
        n.parse::<i64>()
            .map_err(|_| Rich::custom(span, "Invalid integer literal."))
    });

    let string = select! {Token::Quoted(s) => s.to_owned()};

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
        .collect::<Vec<_>>();

    let call = args.delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let intrinsic = just(Token::At)
        .ignore_then(identifier)
        .then(call)
        .try_map(|(id, call), span| match id {
            "print" => Ok(Intrinsic::Print(call)),
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

    expression.define(choice((intrinsic, equality)).labelled("expression"));

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
        .or(block.map_with(|block, extra| Spanned(Statement::Block(block), extra.span())))
        .or(if_statement),
    );

    let program = statement.repeated().collect::<Vec<_>>();

    program.map(|statements| Program { statements })
}
