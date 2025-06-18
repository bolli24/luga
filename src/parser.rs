use crate::lexer::Token;
use chumsky::{input::ValueInput, prelude::*};

#[derive(Clone, Debug)]
pub struct Program {
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub enum Statement {
    Expression(Expression),
}

#[derive(Clone, Debug)]
pub enum Expression {
    Number(f64),
    String(String),
    Intrinsic(Intrinsic),
}

#[derive(Clone, Debug)]
pub enum Intrinsic {
    Print(Vec<Expression>),
}

pub fn luga_parser<'a, I>() -> impl Parser<'a, I, Program, extra::Err<Rich<'a, Token<'a>>>> + Clone
where
    I: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan>,
{
    let identifier = select! {
        Token::Id(str) => str
    };

    let number = select! {Token::Number(n) => n}.try_map(|n, span| {
        n.parse::<f64>()
            .map_err(|_| Rich::custom(span, "Invalid number."))
    });

    let string = select! {Token::Quoted(s) => s.to_owned()};

    let mut expression = Recursive::declare();

    let args = expression
        .clone()
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>();

    let call = args.delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let intrinsic =
        just(Token::At)
            .ignore_then(identifier)
            .then(call)
            .try_map(|(id, call), span| match id {
                "print" => Ok(Intrinsic::Print(call)),
                _ => Err(Rich::custom(span, "Unknown intrinsic.")),
            });

    expression.define(choice((
        intrinsic.map(Expression::Intrinsic),
        number.map(Expression::Number),
        string.map(Expression::String),
    )));

    let statement = expression
        .map(Statement::Expression)
        .then_ignore(just(Token::Semicolon));

    let program = statement.repeated().collect::<Vec<_>>();

    program.map(|statements| Program { statements })
}
