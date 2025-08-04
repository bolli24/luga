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
    If(If),
    While(Conditional),
    FunctionDecl(FunctionDecl),
    StructDecl(StructDecl),
}

#[derive(Clone, Debug)]
pub struct If {
    pub if_branches: Vec<Conditional>,
    pub else_branch: Option<Vec<Spanned<Statement>>>,
}

#[derive(Clone, Debug)]
pub struct Conditional {
    pub condition: Box<Spanned<Expression>>,
    pub block: Vec<Spanned<Statement>>,
}

#[derive(Clone, Debug)]
pub struct FunctionDecl {
    pub name: String,
    pub r#extern: bool,
    pub params: Vec<Param>,
    pub body: Vec<Spanned<Statement>>,
    pub return_type: Option<Spanned<TypeName>>,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Spanned<String>,
    pub type_name: Spanned<TypeName>,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: Spanned<String>,
    pub fields: Vec<(Spanned<String>, Spanned<TypeName>)>,
}

#[derive(Clone, Debug)]
pub struct StructInit {
    pub name: Spanned<String>,
    pub args: Vec<(Spanned<String>, Box<Spanned<Expression>>)>,
}

type TypeName = String;

#[derive(Clone, Debug)]
pub enum Expression {
    Identifier(String),
    Float(f64),
    Integer(i64), // TODO: allow u64
    String(String),
    Bool(bool),

    Binary(BinOp, Box<Spanned<Expression>>, Box<Spanned<Expression>>),
    Unary(UnOp, Box<Spanned<Expression>>),

    Intrinsic(Intrinsic, Vec<Spanned<Expression>>),
    FunctionCall(Spanned<String>, Vec<Spanned<Expression>>),
    FieldAccess(Box<Spanned<Expression>>, Spanned<String>),
    StructInit(StructInit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Equal,
    Lt,
    Gt,
    Lte,
    Gte,
}

impl Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Equal => write!(f, "=="),
            Self::Lt => write!(f, "<"),
            Self::Gt => write!(f, ">"),
            Self::Lte => write!(f, "<="),
            Self::Gte => write!(f, ">="),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
}

impl Display for UnOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Not => write!(f, "!"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Intrinsic {
    Print,
    Println,
    Assert,
    Exit,
}

impl Intrinsic {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Print => "@print",
            Self::Println => "@println",
            Self::Assert => "@assert",
            Self::Exit => "@exit",
        }
    }
}

const KEY_WORDS: [&str; 9] = [
    "let", "return", "if", "else", "true", "false", "fn", "struct", "extern",
];

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
        Token::Lt => BinOp::Lt,
        Token::Gt => BinOp::Gt,
        Token::ArrowLeft => BinOp::Lte,
        Token::ArrowRight => BinOp::Gte,
    };

    let unary_op = select! {
        Token::Bang => UnOp::Not,
    };

    let mut expression = Recursive::declare();

    let args = expression
        .clone()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let function_call = identifier_spanned
        .then(args.clone())
        .map_with(|(name, args), extra| {
            Spanned(Expression::FunctionCall(name, args), extra.span())
        });

    let struct_init = identifier_spanned
        .then(
            identifier_spanned
                .then_ignore(just(Token::Colon))
                .then(expression.clone().map(Box::new))
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose)),
        )
        .map(|(name, args)| Expression::StructInit(StructInit { name, args }));

    let atom = choice((
        function_call.map(|fc| fc.0),
        struct_init,
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

    let field_access = atom.clone().foldl_with(
        just(Token::Dot).ignore_then(identifier_spanned).repeated(),
        |expr, field, extra| Spanned(Expression::FieldAccess(Box::new(expr), field), extra.span()),
    );

    let unary = unary_op
        .repeated()
        .foldr_with(field_access, |op, expr, extras| {
            Spanned(Expression::Unary(op, Box::new(expr)), extras.span())
        });

    let factor = unary.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Mul | BinOp::Div))
            .then(unary.clone())
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

    let comparision = term.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte))
            .then(term.clone())
            .repeated(),
        |a, (op, b), extra| {
            Spanned(
                Expression::Binary(op, Box::new(a), Box::new(b)),
                extra.span(),
            )
        },
    );

    let equality = comparision.clone().foldl_with(
        bin_op
            .filter(|op| matches!(op, BinOp::Equal))
            .then(comparision.clone())
            .repeated(),
        |a, (op, b), extra| {
            Spanned(
                Expression::Binary(op, Box::new(a), Box::new(b)),
                extra.span(),
            )
        },
    );

    let binary = equality;

    let mut statement = Recursive::declare();

    let block = statement
        .clone()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::BraceOpen), just(Token::BraceClose));

    let decl_params = (identifier_spanned
        .then_ignore(just(Token::Colon))
        .then(identifier_spanned)
        .map(|(name, type_name)| Param { name, type_name }))
    .separated_by(just(Token::Comma))
    .collect()
    .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let extern_function_decl = just(Token::Id("extern"))
        .ignore_then(just(Token::Id("fn")))
        .ignore_then(identifier)
        .then(decl_params.clone())
        .then(just(Token::Colon).ignore_then(identifier_spanned).or_not())
        .then_ignore(just(Token::Semicolon))
        .map(|((name, params), return_type)| FunctionDecl {
            name: name.to_owned(),
            r#extern: true,
            params,
            body: Vec::new(),
            return_type,
        });

    let function_decl = just(Token::Id("fn"))
        .ignore_then(identifier)
        .then(decl_params)
        .then(just(Token::Colon).ignore_then(identifier_spanned).or_not())
        .then(block.clone())
        .map(|(((name, params), return_type), body)| FunctionDecl {
            name: name.to_owned(),
            r#extern: false,
            params,
            body,
            return_type,
        })
        .or(extern_function_decl);

    let struct_decl = just(Token::Id("struct"))
        .ignore_then(identifier_spanned)
        .then(
            identifier_spanned
                .then_ignore(just(Token::Colon))
                .then(identifier_spanned)
                .then_ignore(just(Token::Semicolon))
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose)),
        )
        .map_with(|(name, fields), extra| {
            Spanned(
                Statement::StructDecl(StructDecl { name, fields }),
                extra.span(),
            )
        });

    let intrinsic = just(Token::At)
        .ignore_then(identifier)
        .try_map(|id, span| match id {
            "print" => Ok(Intrinsic::Print),
            "println" => Ok(Intrinsic::Println),
            "assert" => Ok(Intrinsic::Assert),
            "exit" => Ok(Intrinsic::Exit),
            _ => Err(Rich::custom(span, "Unknown intrinsic.")),
        })
        .then(args)
        .map_with(|(intrinsic, args), extra| {
            Spanned(Expression::Intrinsic(intrinsic, args), extra.span())
        });

    let else_block = just(Token::Id("else")).ignore_then(block.clone());

    let simple_if = just(Token::Id("if"))
        .ignore_then(
            expression
                .clone()
                .delimited_by(just(Token::ParenOpen), just(Token::ParenClose)),
        )
        .then(block.clone())
        .map(|(condition, block)| Conditional {
            condition: condition.into(),
            block,
        });

    // TODO: If expression
    let if_statement = simple_if
        .separated_by(just(Token::Id("else")))
        .at_least(1)
        .collect::<Vec<_>>()
        .then(else_block.or_not())
        .map_with(|(if_branches, else_branch), extra| {
            Spanned(
                Statement::If(If {
                    if_branches,
                    else_branch,
                }),
                extra.span(),
            )
        });

    let while_statement = just(Token::Id("while"))
        .ignore_then(
            expression
                .clone()
                .delimited_by(just(Token::ParenOpen), just(Token::ParenClose)),
        )
        .then(block.clone())
        .map_with(|(condition, block), extra| {
            Spanned(
                Statement::While(Conditional {
                    condition: condition.into(),
                    block,
                }),
                extra.span(),
            )
        });

    expression.define(choice((intrinsic, binary)).labelled("expression"));
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
            while_statement,
            block.map_with(|block, extra| Spanned(Statement::Block(block), extra.span())),
            function_decl
                .map_with(|decl, extra| Spanned(Statement::FunctionDecl(decl), extra.span())),
            struct_decl,
        ))),
    );

    let program = statement.repeated().collect::<Vec<_>>();

    program.map(|statements| Program { statements })
}
