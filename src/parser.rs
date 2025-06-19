use crate::lexer::Token;
use chumsky::{input::ValueInput, prelude::*};

#[derive(Clone, Debug)]
pub struct Program {
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub enum Statement {
    Expression(Expression),
    Binding(String, Expression),
    Assignment(String, Expression),
    Return(Expression),
}

#[derive(Clone, Debug)]
pub enum Expression {
    Identifier(String),
    Float(f64),
    Integer(i64),
    String(String),
    Intrinsic(Intrinsic),
}

#[derive(Clone, Debug)]
pub enum Intrinsic {
    Print(Vec<Expression>),
}

const KEY_WORDS: [&str; 2] = ["let", "return"];

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

    expression.define(
        choice((
            identifier.map(|name| Expression::Identifier(name.to_owned())),
            intrinsic.map(Expression::Intrinsic),
            float.map(Expression::Float),
            integer.map(Expression::Integer),
            string.map(Expression::String),
        ))
        .labelled("expression"),
    );

    let binding = just(Token::Id("let"))
        .ignore_then(identifier)
        .then_ignore(just(Token::Equal))
        .then(expression.clone());

    let assignment = identifier
        .then_ignore(just(Token::Equal))
        .then(expression.clone());

    let r#return = just(Token::Id("return")).ignore_then(expression.clone());

    let statement = choice((
        r#return.map(Statement::Return),
        binding.map(|(a, b)| Statement::Binding(a.to_owned(), b)),
        assignment.map(|(a, b)| Statement::Assignment(a.to_owned(), b)),
        expression.map(Statement::Expression),
    ))
    .then_ignore(just(Token::Semicolon));

    let program = statement.repeated().collect::<Vec<_>>();

    program.map(|statements| Program { statements })
}
