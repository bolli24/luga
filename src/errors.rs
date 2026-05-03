use std::path::PathBuf;

use ariadne::Source;
use cranelift::codegen::verifier::VerifierError;
use luga_syntax::{
    BinOp, ParseFailure, SimpleSpan, Spanned, UnOp, lexer::LexingError, parsing::report_error,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum CodeError {
    #[error("Invalid type of return value. Exprected '{}' but found: '{}'", .0, .1)]
    InvalidReturnType(String, String),

    #[error("{} isn't supported yet.", .0)]
    NoYetSupported(String),

    #[error("Incompatiple types for binary operator '{}': '{}' and '{}'", .0, .1, .2)]
    IncompatipleBinOpTypes(BinOp, String, String),
    #[error("Invalid type '{}' for binary operator '{}'", .1 ,.0)]
    InvalidBinOpType(BinOp, String),
    #[error("Invalid type '{}' for unary operator '{}'", .1 ,.0)]
    InvalidUnOpType(UnOp, String),

    #[error("Intrisic '@{}' invoked with too few arguments. Expected at least {} but found {}.", .0, .1, .2)]
    IntrinsicTooFewArguments(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked with too many arguments. Expected at most {} but found {}.", .0, .1, .2)]
    IntrinsicTooManyArguments(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked wrong number of arguments. Expected exactly {} but found {}.", .0, .1, .2)]
    IntrinsicArgumentCount(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked with wrong argument type. Expected one of '{}' but found '{}'.", .0, join(.1), .2)]
    IntrisicArgumentType(&'static str, &'static [&'static str], String),
    #[error("Intrinsic '@{}' expected an enum type but found '{}'.", .0, .1)]
    IntrinsicExpectedEnum(&'static str, String),

    #[error("Expected string literal.")]
    ExpectedStringLiteral,

    #[error("Redefinition of existing variable '{}'.", .0)]
    VariableRedefinition(String),
    #[error("Use of unknown variable '{}'.", .0)]
    UnknownVariable(String),
    #[error("Can't assign value of type '{}' to variable of type '{}'.", .1, .0)]
    AssignmentIncompatibleTypes(String, String),
    #[error("Can't create variable of type '{}'.", .0)]
    BindingType(String),
    #[error("Can't capture variable '{0}' outside of function scope")]
    Capture(String),

    #[error("Invalid type '{}' for condition. Must be 'bool'.", .0)]
    InvalidConditionType(String),
    #[error("All branches of an if expression must have same type.")]
    IfExpressionType,

    #[error("Unknown type '{}'.", .0)]
    UnknownType(String),
    #[error("Type '{}' already declared.", .0)]
    TypeRedeclaration(String),
    #[error("Can't coerse integer literal of value '{}' into integer type '{}'", .0, .0)]
    CoerseInt(String, i64),

    #[error("Duplicate function parameter '{}'", .0)]
    FunctionParamDuplicate(String),
    #[error("Function '{}' already declared.", .0)]
    FunctionRedeclaration(String),
    #[error("Name '{}' already defined.", .0)]
    NameAlreadyDefined(String),
    #[error("Call to unknown function '{}'.", .0)]
    UnknownFunction(String),
    #[error("Function '{}' called with wrong number of arguments. Expected {} but found {}.", .0, .1, .2)]
    FunctionCallArgumentCount(String, usize, usize),
    #[error("Function '{}' called with wrong argument type. Expected '{}' but found '{}'.", .0, .1, .2)]
    FunctionCallArgumentType(String, String, String),

    #[error("Unreachable statement.")]
    UnreachableStatement,
    #[error("Function does not return on all code paths.")]
    MissingReturnStatement,

    #[error("Struct field '{}' already declared.", .0)]
    StructDuplicateField(String),
    #[error("Type '{}' has no field named '{}'.", .0, .1)]
    UnknownField(String, String),
    #[error("Type '{}' is not a struct type.", .0)]
    NotStructType(String),
    #[error("Duplicate initialization of field '{}'.", .0)]
    StructInitArgDuplicate(String),
    #[error("Missing initialization of field '{}'.", .0)]
    StructInitMissingArg(String),
    #[error("Invalid type for field initialization. Expected '{}' but found '{}'.", .0, .1)]
    StructInitInvalidType(String, String),

    #[error("Enum variant '{}' already declared.", .0)]
    EnumDuplicateVariant(String),
    #[error("Enum '{}' has no variant named '{}'.", .0, .1)]
    UnknownVariant(String, String),

    #[error("`fn main` must have no parameters.")]
    MainHasParams,
    #[error("`fn main` must return `i32`, but `{}` was declared.", .0)]
    MainInvalidReturnType(String),
    #[error("Type '{}' cannot be used as a value.", .0)]
    TypeUsedAsValue(String),
    #[error("Module '{}' cannot be used as a value.", .0)]
    ModuleUsedAsValue(String),
    #[error("Function '{}' cannot be used as a value.", .0)]
    FunctionUsedAsValue(String),
    #[error("Failed to import '{}': {}", .0, .1)]
    ImportError(String, String),
    #[error("Import not found: '{}'.", .0)]
    ImportNotFound(String),
    #[error("Circular import of '{}'.", .0)]
    CircularImport(String),
}

impl CodeError {
    pub(crate) fn err<T>(self, span: SimpleSpan) -> Result<T, ManyErrors> {
        Err(Spanned(self, span).into())
    }

    pub(crate) const fn spanned(self, span: SimpleSpan) -> Spanned<Self> {
        Spanned(self, span)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ManyErrors(Vec<Spanned<CodeError>>);

impl ManyErrors {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }
}

impl From<Spanned<CodeError>> for ManyErrors {
    fn from(value: Spanned<CodeError>) -> Self {
        Self(vec![value])
    }
}

impl From<Vec<Spanned<CodeError>>> for ManyErrors {
    fn from(value: Vec<Spanned<CodeError>>) -> Self {
        Self(value)
    }
}

impl FromIterator<Spanned<CodeError>> for ManyErrors {
    fn from_iter<T: IntoIterator<Item = Spanned<CodeError>>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for ManyErrors {
    type Item = Spanned<CodeError>;
    type IntoIter = std::vec::IntoIter<Spanned<CodeError>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl std::ops::Deref for ManyErrors {
    type Target = Vec<Spanned<CodeError>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ManyErrors {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub enum GeneratorErrors {
    ModuleErrors(Vec<ModuleErrors>),
    VerifierErrors(Vec<VerifierError>),
    MissingMain,
}

pub enum MainModuleError {
    MissingMain,
    CodeErrors(ManyErrors),
}

#[derive(Debug, Clone)]
pub enum CompileError {
    Code(Spanned<CodeError>),
    ModuleInner,
    Lex(Spanned<LexingError>),
    Parse(Spanned<String>),
}

impl CompileError {
    const fn spanned(self, span: SimpleSpan) -> Spanned<Self> {
        Spanned(self, span)
    }
}

impl From<Spanned<CodeError>> for CompileError {
    fn from(val: Spanned<CodeError>) -> Self {
        CompileError::Code(val)
    }
}

impl CompileError {
    pub(crate) fn from_parse_failure(value: ParseFailure) -> Vec<Self> {
        match value {
            ParseFailure::Lex(errors) => errors.into_iter().map(Self::Lex).collect(),
            ParseFailure::Parse(errors) => errors.into_iter().map(Self::Parse).collect(),
        }
    }
}

pub struct ModuleErrors {
    pub path: PathBuf,
    pub src: String,
    pub errors: Vec<CompileError>,
}

pub fn report_compile_errors(
    file_name: &str,
    src: &Source<&str>,
    errors: impl IntoIterator<Item = CompileError>,
) {
    for err in errors {
        match err {
            CompileError::Code(err) => report_error(file_name, src, &err),
            CompileError::Lex(err) => report_error(file_name, src, &err),
            CompileError::Parse(err) => report_error(file_name, src, &err),
            CompileError::ModuleInner => {}
        }
    }
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}
