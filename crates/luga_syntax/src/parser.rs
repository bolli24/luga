use std::fmt;
use std::fmt::Display;

use crate::lexer::Token;
use chumsky::{input::ValueInput, prelude::*};

#[derive(Debug, Clone)]
pub struct Spanned<T: std::fmt::Debug + Clone>(pub T, pub SimpleSpan);

#[derive(Clone, Debug)]
pub struct Ast {
    pub items: Vec<Spanned<TopLevelItem>>,
}

#[derive(Clone, Debug)]
pub enum TopLevelItem {
    FunctionDecl(FunctionDecl),
    StructDecl(StructDecl),
    EnumDecl(EnumDecl),
    Import(Vec<Spanned<String>>, Option<Spanned<String>>),
}

#[derive(Clone, Debug)]
pub enum Statement {
    Expression(Spanned<Expression>),
    Binding(String, Option<Spanned<String>>, Spanned<Expression>),
    Assignment(Spanned<Expression>, Spanned<Expression>),
    Return(Spanned<Expression>),
    While(Conditional),
    FunctionDecl(FunctionDecl),
    StructDecl(StructDecl),
    EnumDecl(EnumDecl),
    Result(Spanned<Expression>),
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
    pub is_ref: bool,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: Spanned<String>,
    pub fields: Vec<(Spanned<String>, Spanned<TypeName>)>,
    pub functions: Vec<Spanned<FunctionDecl>>,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: Spanned<String>,
    pub variants: Vec<Spanned<String>>,
}

#[derive(Clone, Debug)]
pub struct StructInit {
    pub path: Vec<Spanned<String>>,
    pub args: Vec<(Spanned<String>, Box<Spanned<Expression>>)>,
}

type TypeName = String;

#[derive(Clone, Debug)]
pub struct Arg {
    pub expr: Spanned<Expression>,
    pub is_ref: bool,
}

#[derive(Clone, Debug)]
pub enum Expression {
    Identifier(String),
    Float(f64),
    Integer(i64), // TODO: allow u64
    String(String),
    Bool(bool),

    Block(Vec<Spanned<Statement>>),
    If(If),

    Binary(BinOp, Box<Spanned<Expression>>, Box<Spanned<Expression>>),
    Unary(UnOp, Box<Spanned<Expression>>),

    Intrinsic(Intrinsic, Vec<Spanned<Expression>>),
    FunctionCall(Box<Spanned<Expression>>, Vec<Spanned<Arg>>),
    MemberAccess(Box<Spanned<Expression>>, Spanned<String>),
    StructInit(StructInit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Equal,
    NotEqual,
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
            Self::NotEqual => write!(f, "!="),
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
    Neg,
}

impl Display for UnOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Not => write!(f, "!"),
            Self::Neg => write!(f, "-"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intrinsic {
    Print,
    Println,
    Assert,
    Exit,
    EnumToInt,
}

impl Display for Intrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Print => write!(f, "print"),
            Self::Println => write!(f, "println"),
            Self::Assert => write!(f, "assert"),
            Self::Exit => write!(f, "exit"),
            Self::EnumToInt => write!(f, "enumToInt"),
        }
    }
}

const KEY_WORDS: [&str; 16] = [
    "var", "return", "if", "else", "then", "while", "true", "false", "fn", "struct", "enum",
    "extern", "import", "as", "self", "ref",
];

pub fn luga_parser<'a, I>() -> impl Parser<'a, I, Ast, extra::Err<Rich<'a, Token<'a>>>> + Clone
where
    I: ValueInput<'a, Token = Token<'a>, Span = SimpleSpan>,
{
    let identifier = select! {
        Token::Id(name) => name
    }
    .filter(|name| !KEY_WORDS.iter().any(|key_word| key_word == name))
    .labelled("identifier");

    let identifier_spanned = identifier.map_with(|id, extra| Spanned(id.to_owned(), extra.span()));

    // Like identifier_spanned but accepts any Token::Id, including keywords.
    // Used for cases where names may collide with reserved words (import paths, struct init paths).
    let any_ident_spanned = select! { Token::Id(name) => name }
        .map_with(|id, extra| Spanned(id.to_owned(), extra.span()));

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
        Token::BangEqual => BinOp::NotEqual,
        Token::Lt => BinOp::Lt,
        Token::Gt => BinOp::Gt,
        Token::LtEq => BinOp::Lte,
        Token::GtEq => BinOp::Gte,
    };

    let unary_op = select! {
        Token::Bang => UnOp::Not,
        Token::Minus => UnOp::Neg,
    };

    let mut expression = Recursive::declare();
    let mut statement = Recursive::declare();

    let block = statement
        .clone()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::BraceOpen), just(Token::BraceClose));

    let else_block = just(Token::Id("else")).ignore_then(block.clone());

    let simple_if = just(Token::Id("if"))
        .ignore_then(expression.clone())
        .then_ignore(just(Token::Id("then")))
        .then(block.clone())
        .map(
            |(condition, block): (Spanned<Expression>, Vec<Spanned<Statement>>)| Conditional {
                condition: Box::new(condition),
                block,
            },
        );

    let if_expr = simple_if
        .separated_by(just(Token::Id("else")))
        .at_least(1)
        .collect::<Vec<_>>()
        .then(else_block.or_not())
        .map(|(if_branches, else_branch)| {
            Expression::If(If {
                if_branches,
                else_branch,
            })
        });

    let args = expression
        .clone()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let fn_args = (just(Token::Id("ref")).or_not().then(expression.clone()))
        .map_with(|(ref_keyword, expr), extra| {
            Spanned(
                Arg {
                    expr,
                    is_ref: ref_keyword.is_some(),
                },
                extra.span(),
            )
        })
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<Spanned<Arg>>>()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let struct_init = identifier_spanned
        .then(
            just(Token::Dot)
                .ignore_then(any_ident_spanned)
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|(head, tail)| {
            let mut path = Vec::with_capacity(tail.len() + 1);
            path.push(head);
            path.extend(tail);
            path
        })
        .then(
            identifier_spanned
                .then_ignore(just(Token::Colon))
                .then(expression.clone().map(Box::new))
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose)),
        )
        .map(|(path, args)| Expression::StructInit(StructInit { path, args }));

    let intrinsic_expr = just(Token::At)
        .ignore_then(identifier)
        .try_map(|id, span| match id {
            "print" => Ok(Intrinsic::Print),
            "println" => Ok(Intrinsic::Println),
            "assert" => Ok(Intrinsic::Assert),
            "exit" => Ok(Intrinsic::Exit),
            "enumToInt" => Ok(Intrinsic::EnumToInt),
            _ => Err(Rich::custom(span, "Unknown intrinsic.")),
        })
        .then(args.clone())
        .map(|(intrinsic, args)| Expression::Intrinsic(intrinsic, args));

    let atom = choice((
        intrinsic_expr,
        struct_init,
        identifier.map(|name| Expression::Identifier(name.to_owned())),
        float.map(Expression::Float),
        integer.map(Expression::Integer),
        string.map(Expression::String),
        bool,
        block.clone().map(Expression::Block),
        if_expr.clone(),
        expression
            .clone()
            .delimited_by(just(Token::ParenOpen), just(Token::ParenClose))
            .map(|expr: Spanned<Expression>| expr.0),
    ))
    .map_with(|expr: Expression, extra| -> Spanned<Expression> { Spanned(expr, extra.span()) });

    enum Postfix {
        Member(Spanned<String>),
        Call(Vec<Spanned<Arg>>),
    }

    let postfix = choice((
        just(Token::Dot)
            .ignore_then(identifier_spanned)
            .map(Postfix::Member),
        fn_args.map(Postfix::Call),
    ));

    let field_access = atom
        .clone()
        .foldl_with(postfix.repeated(), |expr, op, extra| match op {
            Postfix::Member(member) => Spanned(
                Expression::MemberAccess(Box::new(expr), member),
                extra.span(),
            ),
            Postfix::Call(call_args) => Spanned(
                Expression::FunctionCall(Box::new(expr), call_args),
                extra.span(),
            ),
        });

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
            .filter(|op| matches!(op, BinOp::Equal | BinOp::NotEqual))
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

    let one_param = just(Token::Id("ref"))
        .or_not()
        .then(identifier_spanned)
        .then_ignore(just(Token::Colon))
        .then(identifier_spanned)
        .map(|((ref_token, name), type_name)| Param {
            name,
            type_name,
            is_ref: ref_token.is_some(),
        });

    let decl_params = one_param
        .separated_by(just(Token::Comma))
        .collect()
        .delimited_by(just(Token::ParenOpen), just(Token::ParenClose));

    let function_decl = choice((
        just(Token::Id("fn"))
            .ignore_then(identifier)
            .then(decl_params.clone())
            .then(just(Token::Colon).ignore_then(identifier_spanned).or_not())
            .then(block.clone())
            .map(|(((name, params), return_type), body)| FunctionDecl {
                name: name.to_owned(),
                r#extern: false,
                params,
                body,
                return_type,
            }),
        just(Token::Id("extern"))
            .ignore_then(just(Token::Id("fn")))
            .ignore_then(identifier)
            .then(decl_params)
            .then(just(Token::Colon).ignore_then(identifier_spanned).or_not())
            .then_ignore(just(Token::Semicolon))
            .map(|((name, params), return_type)| FunctionDecl {
                name: name.to_owned(),
                r#extern: true,
                params,
                body: Vec::new(),
                return_type,
            }),
    ));

    let struct_decl = just(Token::Id("struct"))
        .ignore_then(identifier_spanned)
        .then(
            identifier_spanned
                .then_ignore(just(Token::Colon))
                .then(identifier_spanned)
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose)),
        )
        .then(
            function_decl
                .clone()
                .map_with(|decl, extra| Spanned(decl, extra.span()))
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose))
                .or_not(),
        )
        .map(|((name, fields), functions)| StructDecl {
            name,
            fields,
            functions: functions.unwrap_or_default(),
        });

    let enum_decl = just(Token::Id("enum"))
        .ignore_then(identifier_spanned)
        .then(
            identifier_spanned
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::BraceOpen), just(Token::BraceClose)),
        )
        .map(|(name, variants)| EnumDecl { name, variants });

    let while_statement = just(Token::Id("while"))
        .ignore_then(expression.clone())
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

    expression.define(binary.labelled("expression"));
    let binding = just(Token::Id("var"))
        .ignore_then(identifier)
        .then(just(Token::Colon).ignore_then(identifier_spanned).or_not())
        .then_ignore(just(Token::Equal))
        .then(expression.clone())
        .map_with(|((ident, type_name), expr), extra| -> Spanned<Statement> {
            Spanned(
                Statement::Binding(ident.to_owned(), type_name, expr),
                extra.span(),
            )
        });

    let assignment = expression
        .clone()
        .then_ignore(just(Token::Equal))
        .then(expression.clone())
        .map_with(|(target, expr), extra| -> Spanned<Statement> {
            Spanned(Statement::Assignment(target, expr), extra.span())
        });

    let r#return = just(Token::Id("return"))
        .ignore_then(expression.clone())
        .map_with(|expr: Spanned<Expression>, extra| -> Spanned<Statement> {
            Spanned(Statement::Return(expr), extra.span())
        });

    let result = just(Token::Equal).ignore_then(expression.clone()).map_with(
        |expr, extra| -> Spanned<Statement> { Spanned(Statement::Result(expr), extra.span()) },
    );

    statement.define(
        choice((
            r#return,
            binding,
            assignment,
            result,
            expression.map_with(|expr: Spanned<Expression>, extra| {
                Spanned(Statement::Expression(expr), extra.span())
            }),
        ))
        .then_ignore(just(Token::Semicolon))
        .or(choice((
            block.map(Expression::Block).map_with(|block_expr, extra| {
                Spanned(
                    Statement::Expression(Spanned(block_expr, extra.span())),
                    extra.span(),
                )
            }),
            if_expr.map_with(|if_expr, extra| {
                Spanned(
                    Statement::Expression(Spanned(if_expr, extra.span())),
                    extra.span(),
                )
            }),
            while_statement,
            function_decl
                .clone()
                .map_with(|decl, extra| Spanned(Statement::FunctionDecl(decl), extra.span())),
            struct_decl
                .clone()
                .map_with(|decl, extra| Spanned(Statement::StructDecl(decl), extra.span())),
            enum_decl
                .clone()
                .map_with(|decl, extra| Spanned(Statement::EnumDecl(decl), extra.span())),
        ))),
    );

    let import = just(Token::Id("import")).ignore_then(
        identifier_spanned
            .then(
                just(Token::Dot)
                    .ignore_then(any_ident_spanned)
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map(|(head, tail)| {
                let mut path = Vec::with_capacity(tail.len() + 1);
                path.push(head);
                path.extend(tail);
                path
            })
            .then(
                just(Token::Id("as"))
                    .ignore_then(identifier_spanned)
                    .or_not(),
            )
            .then_ignore(just(Token::Semicolon))
            .map_with(|(path, rename), extra| {
                Spanned(TopLevelItem::Import(path, rename), extra.span())
            }),
    );

    let top_level_item = choice((
        function_decl
            .map_with(|decl, extra| Spanned(TopLevelItem::FunctionDecl(decl), extra.span())),
        struct_decl.map_with(|decl, extra| Spanned(TopLevelItem::StructDecl(decl), extra.span())),
        enum_decl.map_with(|decl, extra| Spanned(TopLevelItem::EnumDecl(decl), extra.span())),
        import,
    ));

    top_level_item
        .repeated()
        .collect::<Vec<_>>()
        .map(|items| Ast { items })
}
