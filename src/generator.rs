use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use chumsky::span::SimpleSpan;
use cranelift::{
    codegen::{Context, verifier::VerifierError, verify_function},
    module::{DataDescription, FuncId, Linkage, Module, default_libcall_names},
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::*,
};

use crate::{
    parser::{BinOp, Expression, FunctionDecl, If, Intrinsic, Param, Program, Spanned, Statement},
    util::ResultCollectAll,
};

const UNREACHABLE: TrapCode = TrapCode::unwrap_user(1);

#[derive(Debug, Clone, thiserror::Error)]
pub enum CodeError {
    #[error("Invalid type of return value. Exprected '{}' but found: '{}'", .0, .1)]
    InvalidReturnType(Type, Type),

    #[error("{} isn't supported yet.", .0)]
    NoYetSupported(String),

    #[error("Incompatiple types for binary operator '{}': '{}' and '{}'", .0, .1, .2)]
    IncompatipleBinOpTypes(BinOp, Type, Type),
    #[error("Invalid type '{}' for binary operator '{}'", .1 ,.0)]
    InvalidBinOpType(BinOp, Type),

    #[error("Intrisic '@{}' invoked with too few arguments. Expected at least {} but found {}.", .0, .1, .2)]
    IntrinsicTooFewArguments(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked with too many arguments. Expected at most {} but found {}.", .0, .1, .2)]
    IntrinsicTooManyArguments(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked wrong number of arguments. Expected exactly {} but found {}.", .0, .1, .2)]
    IntrinsicArgumentCount(&'static str, usize, usize),
    #[error("Intrisic '@{}' invoked with wrong argument type. Expected one of '{}' but found '{}'.", .0, join(.1), .2)]
    IntrisicArgumentType(&'static str, &'static [Type], Type),

    #[error("Expected string literal.")]
    ExpectedStringLiteral,

    #[error("Redefinition of existing variable '{}'.", .0)]
    VariableRedefinition(String),
    #[error("Use of unknown variable '{}'.", .0)]
    UnknownVariable(String),
    #[error("Can't assign value of type '{}' to variable of type '{}'.", .1, .0)]
    AssignmentIncompatibleTypes(Type, Type),
    #[error("Can't create variable of type '{}'.", .0)]
    BindingType(Type),

    #[error("Invalid type '{}' for condition. Must be 'bool'.", .0)]
    InvalidConditionType(Type),
    #[error("All branches of an if expression must have same type.")]
    IfExpressionType,

    #[error("Unknown type '{}'.", .0)]
    UnknownType(String),

    #[error("Duplicate function parameter '{}'", .0)]
    FunctionParamDuplicate(String),
    #[error("Function '{}' already declared.", .0)]
    FunctionRedeclaration(String),
    #[error("Call to unknown function '{}'.",.0)]
    UnknownFunction(String),
    #[error("Function '{}' called with wrong number of arguments. Expected {} but found {}.", .0, .1, .2)]
    FunctionCallArgumentCount(String, usize, usize),
    #[error("Function '{}' called with wrong argument type. Expected '{}' but found '{}'.", .0, .1, .2)]
    FunctionCallArgumentType(String, Type, Type),

    #[error("Unreachable statement.")]
    UnreachableStatement,
    #[error("Function does not return on all code paths.")]
    MissingReturnStatement,
    #[error("Must declare function in top level scope.")]
    FunctionDeclNotTopLevel,
}

impl From<Spanned<CodeError>> for ManyErrors {
    fn from(value: Spanned<CodeError>) -> Self {
        vec![value]
    }
}

impl CodeError {
    fn err<T>(self, span: SimpleSpan) -> Result<T, Vec<Spanned<Self>>> {
        Err(Spanned(self, span).into())
    }

    const fn spanned(self, span: SimpleSpan) -> Spanned<Self> {
        Spanned(self, span)
    }
}

pub enum GeneratorErrors {
    CodeErrors(ManyErrors),
    VerifierErrors(Vec<VerifierError>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
    Void,
    Never,
    String,
    Bool,
    Fn(FuncType),
    // TODO: Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    name: String,
    params: Vec<Type>,
    return_type: Box<Type>,
}

impl TryFrom<&Type> for types::Type {
    type Error = ();
    fn try_from(val: &Type) -> Result<Self, Self::Error> {
        Ok(match val {
            Type::Int => types::I64,
            Type::Float => types::F64,
            Type::String => types::I64, // TODO: platform ptr size
            Type::Bool => types::I8,
            _ => return Err(()),
        })
    }
}

impl TryFrom<&Type> for AbiParam {
    type Error = ();
    fn try_from(value: &Type) -> Result<Self, Self::Error> {
        Ok(Self::new(value.try_into()?))
    }
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int => write!(f, "integer"),
            Self::Float => write!(f, "float"),
            Self::Void => write!(f, "void"),
            Self::Never => write!(f, "never"),
            Self::String => write!(f, "string"),
            Self::Bool => write!(f, "bool"),
            // TODO: Type::Named(name) => write!(f, "{name}"),
            Self::Fn(func_type) => write!(f, "{func_type}"),
        }
    }
}

impl Display for FuncType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fn {} ({}) : {}",
            self.name,
            self.params
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            self.return_type
        )
    }
}

pub fn generate(
    program: Program,
    verbose: bool,
    release: bool,
) -> Result<ObjectProduct, GeneratorErrors> {
    #[cfg(target_os = "linux")]
    const TARGET_TRIPLE: &str = "x86_64-unknown-linux";

    #[cfg(target_os = "windows")]
    const TARGET_TRIPLE: &str = "x86_64-pc-windows-gnu";

    let isa = {
        let mut builder = settings::builder();

        // disable optimizations so dissassembly will more directly correlated to our Cranelift usage
        builder
            .set("opt_level", if release { "speed" } else { "none" })
            .unwrap();

        builder.enable("is_pic").unwrap();

        let flags = settings::Flags::new(builder);

        isa::lookup_by_name(TARGET_TRIPLE)
            .unwrap()
            .finish(flags)
            .unwrap()
    };

    let mut module = {
        let translation_unit_name = b"output_a_binary";
        let libcall_names = default_libcall_names();
        let builder =
            ObjectBuilder::new(isa.clone(), translation_unit_name, libcall_names).unwrap();
        ObjectModule::new(builder)
    };
    let mut main_context = Context::new();

    let mut data_desc = DataDescription::new();
    let string_data = b"%d\n\0";
    data_desc.define(string_data.to_vec().into_boxed_slice());
    let string_data_id = module
        .declare_data("format_string", Linkage::Local, true, false)
        .expect("Failed to declare string data");

    module
        .define_data(string_data_id, &data_desc)
        .expect("Failed to define string data");

    let main_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![],
        // Since we're linking to libc, we can return the exit code from main.
        returns: vec![AbiParam::new(types::I32)],
    };

    let main_id = module
        .declare_function("main", Linkage::Export, &main_signature)
        .unwrap();

    let mut main_fn_context = FunctionBuilderContext::new();

    let mut main_builder = FunctionBuilder::new(&mut main_context.func, &mut main_fn_context);
    main_builder.func.signature = main_signature;

    let main_block = main_builder.create_block();
    main_builder.switch_to_block(main_block);
    main_builder.seal_block(main_block);

    let printf_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![AbiParam::new(types::I64)],
        returns: vec![AbiParam::new(types::I32)],
    };

    let printf_id = module
        .declare_function("printf", Linkage::Import, &printf_signature)
        .unwrap();

    let exit_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![AbiParam::new(types::I32)],
        returns: vec![],
    };

    let exit_id = module
        .declare_function("exit", Linkage::Import, &exit_signature)
        .unwrap();

    let generator = Generator {
        isa: isa.clone(),
        fn_context: Context::new(),
        fn_builder_context: FunctionBuilderContext::new(),
        builder: main_builder,
        module: &mut module,
        strings: 0,
        printf_id,
        exit_id,
        returned: false,
        current_scope: Scope::new(),
        var_count: 0,
        verbose,
        current_func_type: FuncType {
            name: "main".to_owned(),
            params: Vec::new(),
            return_type: Type::Int.into(), // TODO: I32
        },
    };

    generator.build(program)?;

    if verbose {
        println!("{}", main_context.func.display());
    }

    let flags = settings::Flags::new(settings::builder());
    if let Err(errs) = verify_function(&main_context.func, &flags) {
        return Err(GeneratorErrors::VerifierErrors(errs.0));
    }

    module.define_function(main_id, &mut main_context).unwrap();

    Ok(module.finish())
}

type ManyErrors = Vec<Spanned<CodeError>>;

struct Scope {
    vars_name: HashMap<String, (Variable, Type)>,
    vars: HashSet<usize>,
    parent: Option<Box<Scope>>,
    functions: HashMap<String, (FuncId, FuncType)>,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            vars_name: HashMap::new(),
            vars: HashSet::new(),
            parent: None,
            functions: HashMap::new(),
        }
    }

    pub fn function_scope(&self) -> Self {
        let mut new_scope = Self::new();
        new_scope.functions = self.functions.clone();
        new_scope
    }

    pub fn get_var(&self, name: &str) -> Option<&(Variable, Type)> {
        self.vars_name
            .get(name)
            .or_else(|| self.parent.as_ref()?.get_var(name))
    }

    pub fn insert(&mut self, name: String, var: Variable, value: (Value, Type)) -> Result<(), ()> {
        use std::collections::hash_map::Entry;

        match self.vars_name.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert((var, value.1));
                self.vars.insert(var.index());
                Ok(())
            }
            Entry::Occupied(_) => Err(()),
        }
    }

    pub fn get_func(&self, name: &str) -> Option<&(FuncId, FuncType)> {
        self.functions
            .get(name)
            .or_else(|| self.parent.as_ref()?.get_func(name))
    }

    #[must_use]
    pub fn insert_func(
        &mut self,
        name: String,
        func_id: FuncId,
        func_type: FuncType,
    ) -> Option<()> {
        if self.parent.is_some() {
            None
        } else {
            self.functions.insert(name, (func_id, func_type));
            Some(())
        }
    }
}

struct Generator<'a> {
    isa: std::sync::Arc<dyn isa::TargetIsa>,
    fn_context: Context,
    fn_builder_context: FunctionBuilderContext,
    builder: FunctionBuilder<'a>,
    module: &'a mut ObjectModule,
    strings: usize,
    printf_id: FuncId,
    exit_id: FuncId,
    returned: bool,
    current_scope: Scope,
    var_count: usize,
    verbose: bool,
    current_func_type: FuncType,
}

impl Generator<'_> {
    fn build(mut self, program: Program) -> Result<(), GeneratorErrors> {
        match self.build_statements(&program.statements) {
            Ok(returned) => {
                if !returned {
                    let zero = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[zero]);
                }
                Ok(())
            }
            Err(errors) => Err(GeneratorErrors::CodeErrors(errors)),
        }
    }

    fn build_statements(&mut self, statements: &[Spanned<Statement>]) -> Result<bool, ManyErrors> {
        let mut errors = Vec::new();
        let mut iter = statements.iter().peekable();
        let mut returned = false;
        while let Some(statement) = iter.next() {
            match self.build_statement(statement) {
                Err(mut error) => {
                    errors.append(&mut error);
                }
                Ok(true) => {
                    if let Some(next) = iter.peek() {
                        errors.push(CodeError::UnreachableStatement.spanned(next.1))
                    }
                    returned = true;
                    break;
                }
                Ok(false) => {}
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(returned)
    }

    fn build_statement(&mut self, statement: &Spanned<Statement>) -> Result<bool, ManyErrors> {
        let mut returned = false;
        match &statement.0 {
            Statement::Return(expr) => {
                let value = self.build_expression(expr)?;

                if self.current_func_type.name == "main" {
                    if value.1 != Type::Int {
                        // self.builder.ins().return_(&[]);
                        return CodeError::InvalidReturnType(Type::Int, value.1).err(expr.1);
                    }
                    let cast = self.builder.ins().ireduce(types::I32, value.0);
                    self.builder.ins().return_(&[cast]);
                } else {
                    if *self.current_func_type.return_type != value.1 {
                        return CodeError::InvalidReturnType(
                            (*self.current_func_type.return_type).clone(),
                            value.1.clone(),
                        )
                        .err(expr.1);
                    }
                    match value.1 {
                        Type::Never => panic!("Trying to return from unreachable code."),
                        Type::Void => self.builder.ins().return_(&[]),
                        _ => self.builder.ins().return_(&[value.0]),
                    };
                }
                returned = true;
            }
            Statement::Expression(expr) => {
                self.build_expression(expr)?;
            }
            Statement::Binding(name, expr) => {
                let value = self.build_expression(expr)?;

                let Ok(var) = self.new_var(name.clone(), value.clone()) else {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                };

                let Ok(var_type) = (&value.1).try_into() else {
                    return CodeError::BindingType(value.1).err(expr.1);
                };

                self.builder.declare_var(var, var_type);
                self.builder.def_var(var, value.0);
            }
            Statement::Assignment(name, expr) => {
                let value = self.build_expression(expr)?;

                if let Some((var, var_type)) = self.current_scope.get_var(name) {
                    if value.1 != *var_type {
                        return CodeError::AssignmentIncompatibleTypes(var_type.clone(), value.1)
                            .err(statement.1);
                    }

                    self.builder.def_var(*var, value.0);
                } else {
                    return CodeError::UnknownVariable(name.clone()).err(statement.1);
                }
            }
            Statement::Block(statements) => {
                let new_block = self.builder.create_block();
                self.builder.ins().jump(new_block, []);
                self.builder.switch_to_block(new_block);
                self.builder.seal_block(new_block);
                self.push_scope();

                let result = self.build_statements(statements);

                if self.pop_scope().is_none() {
                    unreachable!("Must always be able to pop scope after leaving block.");
                }

                let new_block = self.builder.create_block();
                self.builder.ins().jump(new_block, []);
                self.builder.switch_to_block(new_block);
                self.builder.seal_block(new_block);

                return result;
            }
            Statement::If(r#if) => {
                return self.build_if(r#if);
            }
            Statement::FunctionDecl(decl) => self.build_fn_decl(decl, &statement.1)?,
        }

        Ok(returned)
    }

    fn build_expression(
        &mut self,
        expression: &Spanned<Expression>,
    ) -> Result<(Value, Type), ManyErrors> {
        let Spanned(expression, span) = expression;
        match expression {
            Expression::Float(value) => Ok((self.builder.ins().f64const(*value), Type::Float)),
            Expression::Integer(value) => {
                Ok((self.builder.ins().iconst(types::I64, *value), Type::Int))
            }
            Expression::String(value) => {
                let mut data_desc = DataDescription::new();
                let mut string_data = value.as_bytes().to_vec();
                string_data.push(0);
                data_desc.define(string_data.into_boxed_slice());

                let string_id = self.strings;
                self.strings += 1;
                let string_data_id = self
                    .module
                    .declare_data(&format!("string_{string_id}"), Linkage::Local, true, false)
                    .expect("Failed to declare string data");

                self.module
                    .define_data(string_data_id, &data_desc)
                    .expect("Failed to define string data");

                let format_string_ref = self
                    .module
                    .declare_data_in_func(string_data_id, self.builder.func);
                let value = self
                    .builder
                    .ins()
                    .global_value(types::I64, format_string_ref);

                Ok((value, Type::String))
            }
            Expression::Bool(value) => Ok((
                self.builder.ins().iconst(types::I8, i64::from(*value)),
                Type::Bool,
            )),
            Expression::Binary(op, x, y) => {
                let x_value = self.build_expression(x.as_ref())?;
                let y_value = self.build_expression(y.as_ref())?;

                if x_value.1 != y_value.1 {
                    return CodeError::IncompatipleBinOpTypes(*op, x_value.1, y_value.1).err(*span);
                }

                let result = match x_value.1 {
                    Type::Int => self.build_int_bin_op(*op, x_value.0, y_value.0),
                    Type::Float => self.build_float_bin_op(*op, x_value.0, y_value.0),
                    Type::Bool => self.build_bool_bin_op(*op, x_value.0, y_value.0),
                    _ => None,
                }
                .ok_or_else(|| CodeError::InvalidBinOpType(*op, x_value.1).spanned(*span))?;
                Ok(result)
            }
            Expression::Intrinsic(intrinsic, args) => match intrinsic {
                Intrinsic::Print => self.build_printf(span, args, false),
                Intrinsic::Println => self.build_printf(span, args, true),
                Intrinsic::Assert => self.build_assert(span, args),
                Intrinsic::Exit => self.build_exit(span, args),
            },
            Expression::Identifier(name) => {
                let Some((var, var_type)) = self.current_scope.get_var(name) else {
                    return CodeError::UnknownVariable(name.to_owned()).err(*span);
                };

                Ok((self.builder.use_var(*var), var_type.clone()))
            }
            Expression::FunctionCall(name, args) => self.build_fn_call(name, args, span),
        }
    }

    #[must_use]
    fn build_int_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some((
            match op {
                BinOp::Add => self.builder.ins().iadd(x, y),
                BinOp::Sub => self.builder.ins().isub(x, y),
                BinOp::Mul => self.builder.ins().imul(x, y),
                BinOp::Div => self.builder.ins().sdiv(x, y),
                BinOp::Equal => self.builder.ins().icmp(IntCC::Equal, x, y),
                BinOp::Lt => self.builder.ins().icmp(IntCC::SignedLessThan, x, y),
                BinOp::Gt => self.builder.ins().icmp(IntCC::SignedGreaterThan, x, y),
                BinOp::Lte => self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, x, y),
                BinOp::Gte => self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, x, y),
            },
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Type::Int,
                BinOp::Equal | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte => Type::Bool,
            },
        ))
    }

    #[must_use]
    fn build_float_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some((
            match op {
                BinOp::Add => self.builder.ins().fadd(x, y),
                BinOp::Sub => self.builder.ins().fsub(x, y),
                BinOp::Mul => self.builder.ins().fmul(x, y),
                BinOp::Div => self.builder.ins().fdiv(x, y),
                BinOp::Equal => self.builder.ins().fcmp(FloatCC::Equal, x, y),
                BinOp::Lt => self.builder.ins().fcmp(FloatCC::LessThan, x, y),
                BinOp::Gt => self.builder.ins().fcmp(FloatCC::GreaterThan, x, y),
                BinOp::Lte => self.builder.ins().fcmp(FloatCC::LessThanOrEqual, x, y),
                BinOp::Gte => self.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, x, y),
            },
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Type::Int,
                BinOp::Equal | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte => Type::Bool,
            },
        ))
    }

    #[must_use]
    fn build_bool_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some(match op {
            BinOp::Equal => (self.builder.ins().icmp(IntCC::Equal, x, y), Type::Bool),
            _ => return None,
        })
    }

    fn build_printf(
        &mut self,
        span: &SimpleSpan,
        args: &[Spanned<Expression>],
        new_line: bool,
    ) -> Result<(Value, Type), ManyErrors> {
        let Some((format_string_arg, other_args)) = args.split_first() else {
            return CodeError::IntrinsicTooFewArguments("print", 1, args.len()).err(*span);
        };

        let mut format_string_arg = format_string_arg.clone();

        // TODO: validate format string/generate own format string
        if let Expression::String(format_string) = &mut format_string_arg.0 {
            if new_line {
                format_string.push('\n');
            }
        } else {
            return CodeError::ExpectedStringLiteral.err(format_string_arg.1);
        };

        let arg_values = other_args
            .iter()
            .map(|expr| {
                let result = self.build_expression(expr);
                if let Ok(value) = &result {
                    const PRINT_ARG_TYPES: [Type; 2] = [Type::Int, Type::String];
                    if !PRINT_ARG_TYPES.contains(&value.1) {
                        return CodeError::IntrisicArgumentType(
                            "print",
                            &PRINT_ARG_TYPES,
                            value.1.clone(),
                        )
                        .err(expr.1);
                    }
                }
                result
            })
            .collect_all()
            .map_err(|errors| errors.into_iter().flatten().collect::<ManyErrors>())?;

        let format_string_value = self.build_expression(&format_string_arg)?;
        debug_assert_eq!(format_string_value.1, Type::String);

        // Because printf is a variadic function we have to change it's signature on the fly
        // See: adjust_call_for_c_variadic() in rustc_codegen_cranelift
        let mut params = Vec::with_capacity(arg_values.len() + 1);
        params.push(AbiParam::new(self.isa.pointer_type()));
        params.extend(
            arg_values
                .iter()
                .map(|(_, t)| AbiParam::try_from(t).expect("Type must be valid @print parameter.")),
        );

        let printf_ref = self
            .module
            .declare_func_in_func(self.printf_id, self.builder.func);

        let sig_ref = self.builder.func.dfg.ext_funcs[printf_ref].signature;
        self.builder.func.dfg.signatures[sig_ref].params = params;

        let mut call_args = Vec::with_capacity(arg_values.len() + 1);
        call_args.push(format_string_value.0);
        call_args.extend(arg_values.iter().map(|(val, _)| *val));

        let result = self.builder.ins().call(printf_ref, &call_args);

        let value = self.builder.func.dfg.inst_results(result)[0];
        // TODO: Type::I32
        Ok((value, Type::Int))
    }

    fn push_scope(&mut self) {
        let old_scope = std::mem::replace(&mut self.current_scope, Scope::new());
        self.current_scope.parent = Some(Box::new(old_scope));
    }

    fn pop_scope(&mut self) -> Option<Scope> {
        if let Some(mut parent) = self.current_scope.parent.take() {
            std::mem::swap(&mut self.current_scope, &mut *parent);
            let child = parent;
            Some(*child)
        } else {
            // No parent to pop to — already at root
            None
        }
    }

    fn new_var(&mut self, name: String, value: (Value, Type)) -> Result<Variable, ()> {
        let var = Variable::new(self.var_count);
        self.current_scope.insert(name, var, value)?;
        self.var_count += 1;
        Ok(var)
    }

    // TODO: if expression
    // TODO: add clif comments to blocks
    fn build_if(&mut self, if_statement: &If) -> Result<bool, ManyErrors> {
        let condition_blocks: Vec<_> = if_statement
            .if_branches
            .iter()
            .map(|_| self.builder.create_block())
            .collect();

        let else_block = if_statement
            .else_branch
            .as_ref()
            .map(|_| self.builder.create_block());

        let exit_block = self.builder.create_block();

        let mut errors = Vec::new();

        self.builder.ins().jump(condition_blocks[0], []);

        let mut returned = true;

        for (index, conditional) in if_statement.if_branches.iter().enumerate() {
            let condition_block = condition_blocks[index];
            let then_block = self.builder.create_block();
            self.builder.seal_block(condition_block);
            self.builder.switch_to_block(condition_block);
            let condition_value = match self.build_expression(&conditional.condition) {
                Ok(condition_value) if condition_value.1 == Type::Bool => condition_value,
                Ok((_, r#type)) => {
                    errors.push(
                        CodeError::InvalidConditionType(r#type).spanned(conditional.condition.1),
                    );
                    continue;
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    continue;
                }
            };

            let next_block: Block = condition_blocks
                .get(index + 1)
                .copied()
                .or(else_block)
                .unwrap_or(exit_block);

            self.builder
                .ins()
                .brif(condition_value.0, then_block, [], next_block, []);

            self.builder.seal_block(then_block);
            self.builder.switch_to_block(then_block);
            match self.build_block_body(&conditional.block) {
                Ok((_, block_type)) => {
                    assert!(block_type == Type::Void || block_type == Type::Never); // TODO: if expression
                    if block_type != Type::Never {
                        self.builder.ins().jump(exit_block, []);
                        returned = false;
                    }
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    self.builder.ins().jump(exit_block, []);
                }
            }
        }

        if let Some(else_block) = else_block {
            self.builder.seal_block(else_block);
            self.builder.switch_to_block(else_block);

            match self.build_block_body(
                if_statement
                    .else_branch
                    .as_ref()
                    .expect("Else branch must exist."),
            ) {
                Ok((_, block_type)) => {
                    assert!(block_type == Type::Void || block_type == Type::Never); // TODO: if expression
                    if block_type != Type::Never {
                        self.builder.ins().jump(exit_block, []);
                        returned = false;
                    }
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    self.builder.ins().jump(exit_block, []);
                }
            }
        }

        self.builder.seal_block(exit_block);
        self.builder.switch_to_block(exit_block);

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(returned)
    }

    fn build_block_body(
        &mut self,
        body: &[Spanned<Statement>],
    ) -> Result<(Value, Type), ManyErrors> {
        self.push_scope();

        let result = self.build_statements(body);

        let Some(_) = self.pop_scope() else {
            unreachable!("Must always be able to pop scope after leaving block.");
        };

        match result {
            Ok(true) => Ok((Value::new((u32::MAX - 1) as usize), Type::Never)),
            Ok(false) => Ok((Value::new((u32::MAX - 1) as usize), Type::Void)),
            Err(errors) => Err(errors),
        }
    }

    fn build_assert(
        &mut self,
        span: &SimpleSpan,
        args: &[Spanned<Expression>],
    ) -> Result<(Value, Type), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("assert", 1, args.len()).err(*span);
        }

        let condition = self.build_expression(&args[0])?;
        if condition.1 != Type::Bool {
            return CodeError::IntrisicArgumentType("assert", &[Type::Bool], condition.1)
                .err(args[0].1);
        }

        let exit_code = self.builder.ins().iconst(types::I32, 1);

        // TODO: use abort instead of exit
        let exit_ref = self
            .module
            .declare_func_in_func(self.exit_id, self.builder.func);

        let exit_block = self.builder.create_block();
        let continue_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(condition.0, continue_block, [], exit_block, []);

        self.builder.seal_block(exit_block);
        self.builder.seal_block(continue_block);

        self.builder.switch_to_block(exit_block);
        self.builder.ins().call(exit_ref, &[exit_code]);
        self.builder.ins().trap(UNREACHABLE);

        self.builder.switch_to_block(continue_block);

        Ok((Value::from_u32(u32::MAX - 1), Type::Void))
    }

    fn build_exit(
        &mut self,
        span: &SimpleSpan,
        args: &[Spanned<Expression>],
    ) -> Result<(Value, Type), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("exit", 1, args.len()).err(*span);
        }

        let expr = self.build_expression(&args[0])?;
        if expr.1 != Type::Int {
            return CodeError::IntrisicArgumentType("exit", &[Type::Bool], expr.1).err(args[0].1);
        }

        let exit_ref = self
            .module
            .declare_func_in_func(self.exit_id, self.builder.func);

        let cast = self.builder.ins().ireduce(types::I32, expr.0);
        self.builder.ins().call(exit_ref, &[cast]);
        Ok((Value::from_u32(u32::MAX - 1), Type::Never))
    }

    fn build_fn_decl(&mut self, decl: &FunctionDecl, span: &SimpleSpan) -> Result<(), ManyErrors> {
        let &FunctionDecl {
            name,
            r#extern,
            params,
            body,
            return_type,
        } = &decl;

        if *r#extern {
            todo!("Extern functions")
        }

        let mut abi_params: Vec<(String, Type)> = Vec::new();
        let mut errors = Vec::new();
        let mut param_types = Vec::new();

        if self.current_scope.get_func(name).is_some() {
            errors.push(CodeError::FunctionRedeclaration(name.to_owned()).spanned(*span));
        }

        for Param { name, type_name } in params {
            let Some(r#type) = self.type_from_name(&type_name.0) else {
                errors.push(CodeError::UnknownType(type_name.0.clone()).spanned(type_name.1));
                continue;
            };

            if abi_params
                .iter()
                .any(|(other_name, _)| &name.0 == other_name)
            {
                errors.push(CodeError::FunctionParamDuplicate(name.0.clone()).spanned(name.1));
            }
            param_types.push(r#type.clone());
            abi_params.push((name.0.clone(), r#type));
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let return_type = match return_type {
            Some(type_name) => self
                .type_from_name(&type_name.0)
                .ok_or(CodeError::UnknownType(type_name.0.clone()).spanned(type_name.1))?,
            None => Type::Void,
        };

        let returns = match &return_type {
            Type::Void => Vec::new(),
            return_type => {
                vec![AbiParam::try_from(return_type).expect("Return type must be valid.")]
            }
        };

        let func_type = FuncType {
            name: name.to_owned(),
            params: param_types,
            return_type: return_type.into(),
        };

        if self.verbose {
            println!("{func_type}");
        }

        let signature = Signature {
            call_conv: self.isa.default_call_conv(),
            params: abi_params
                .iter()
                .map(|(_, r#type)| r#type.try_into().expect("Param type must be valid."))
                .collect(),
            returns,
        };

        let func_id = self
            .module
            .declare_function(name, Linkage::Local, &signature)
            .expect("Function must be declarable.");

        self.current_scope
            .insert_func(name.to_owned(), func_id, func_type.clone())
            .ok_or(CodeError::FunctionDeclNotTopLevel.spanned(*span))?;

        let mut fn_context = Context::new();
        let mut fn_builder_context = FunctionBuilderContext::new();
        let mut fn_builder = FunctionBuilder::new(&mut fn_context.func, &mut fn_builder_context);

        fn_builder.func.signature = signature;

        let entry_block = fn_builder.create_block();
        fn_builder.append_block_params_for_function_params(entry_block);
        fn_builder.switch_to_block(entry_block);
        fn_builder.seal_block(entry_block);

        let mut fn_generator = Generator {
            isa: self.isa.clone(),
            fn_context: Context::new(), // This won't be used for the function itself
            fn_builder_context: FunctionBuilderContext::new(), // This won't be used either
            builder: fn_builder,
            module: self.module,
            strings: self.strings, // Share string counter
            printf_id: self.printf_id,
            exit_id: self.exit_id,
            returned: false,
            current_scope: self.current_scope.function_scope(),
            var_count: 0,
            verbose: self.verbose,
            current_func_type: func_type.clone(),
        };

        for (i, (param_name, param_type)) in abi_params.iter().enumerate() {
            let param_value = fn_generator.builder.block_params(entry_block)[i];

            let var = fn_generator
                .new_var(param_name.clone(), (param_value, param_type.clone()))
                .expect("Function parameters should never redefine variables");

            let var_type = param_type
                .try_into()
                .expect("Parameter type should be valid");

            fn_generator.builder.declare_var(var, var_type);
            fn_generator.builder.def_var(var, param_value);
        }

        match fn_generator.build_statements(body) {
            Err(mut new_errors) => {
                errors.append(&mut new_errors);
            }
            Ok(false) if *func_type.return_type == Type::Void => {
                fn_generator.builder.ins().return_(&[]);
            }
            Ok(false) => errors.push(CodeError::MissingReturnStatement.spanned(*span)),
            Ok(true) => {}
        }

        self.strings = fn_generator.strings;

        if !errors.is_empty() {
            return Err(errors);
        }

        if self.verbose {
            println!("{}", fn_generator.builder.func.display());
        }

        self.module
            .define_function(func_id, &mut fn_context)
            .unwrap();

        Ok(())
    }

    fn type_from_name(&self, type_name: &str) -> Option<Type> {
        Some(match type_name {
            "int" => Type::Int,
            "float" => Type::Float,
            "bool" => Type::Bool,
            "string" => Type::String,
            "void" => Type::Void,
            _ => return None,
        })
    }

    fn build_fn_call(
        &mut self,
        name: &Spanned<String>,
        args: &[Spanned<Expression>],
        span: &SimpleSpan,
    ) -> Result<(Value, Type), ManyErrors> {
        let Some((func_id, func_type)) = self.current_scope.get_func(&name.0).cloned() else {
            return CodeError::UnknownFunction(name.0.clone()).err(*span);
        };

        if args.len() != func_type.params.len() {
            return CodeError::FunctionCallArgumentCount(
                name.0.clone(),
                func_type.params.len(),
                args.len(),
            )
            .err(*span);
        }

        let arg_values = args
            .iter()
            .zip(func_type.params.iter())
            .map(|(expr, param_type)| {
                let value = self.build_expression(expr)?;
                if value.1 != *param_type {
                    return CodeError::FunctionCallArgumentType(
                        name.0.clone(),
                        param_type.clone(),
                        value.1,
                    )
                    .err(*span);
                }
                Ok(value.0)
            })
            .collect::<Result<Vec<Value>, ManyErrors>>()?;

        let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
        let inst = self.builder.ins().call(func_ref, &arg_values);
        match self.builder.inst_results(inst) {
            [] => Ok((Value::from_u32(u32::MAX - 1), Type::Void)),
            [value] => Ok((*value, *func_type.return_type)),
            _ => panic!("Invalid number of return values. Must be 0 or 1."),
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
