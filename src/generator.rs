use std::{collections::HashMap, fmt::Display};

use chumsky::span::SimpleSpan;
use cranelift::{
    codegen::{Context, verifier::VerifierError, verify_function},
    prelude::*,
};
use cranelift_codegen::{ir::FuncRef, isa::TargetIsa};
use cranelift_module::{DataDescription, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};

use crate::{
    parser::{BinOp, Expression, Intrinsic, Program, Spanned, Statement},
    util::ResultCollectAll,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum CodeError {
    #[error("Return value type must be integer, found: {}", .0)]
    InvalidReturnType(Type),

    #[error("{} isn't supported yet.", .0)]
    NoYetSupported(String),

    #[error("Incompatiple types for binary operator '{}': '{}' and '{}'", .0, .1, .2)]
    IncompatipleBinOpTypes(BinOp, Type, Type),
    #[error("Invalid type '{}' for binary operator '{}'", .0 ,.1)]
    InvalidBinOpType(BinOp, Type),

    #[error("Intrisic '{}' invoked with too few arguments. Expected at least {} but found {}.", .0, .1, .2)]
    IntrinsicTooFewArguments(&'static str, usize, usize),
    #[error("Intrisic '{}' invoked with too many arguments. Expected at most {} but found {}.", .0, .1, .2)]
    IntrinsicTooManyArguments(&'static str, usize, usize),
    #[error("Intrisic '{}' invoked with wrong argument type. Expected one of '{}' but found '{}'.", .0, join(.1), .2)]
    IntrisicArgumentType(&'static str, &'static [Type], Type),

    #[error("Expected string literal.")]
    ExpectedStringLiteral,

    #[error("Redefinition of existing variable '{}'.", .0)]
    VariableRedefinition(String),
    #[error("Use of unknown variable '{}'.", .0)]
    UnknownVariable(String),
    #[error("Can't assign value of type '{}' to variable of type '{}'.", .1, .0)]
    AssignmentIncompatibleTypes(Type, Type),
}

impl From<Spanned<CodeError>> for Vec<Spanned<CodeError>> {
    fn from(value: Spanned<CodeError>) -> Self {
        vec![value]
    }
}

impl CodeError {
    fn err<T>(self, span: SimpleSpan) -> Result<T, Vec<Spanned<Self>>> {
        Err(Spanned(self, span).into())
    }
}

pub enum GeneratorErrors {
    CodeErrors(Vec<Spanned<CodeError>>),
    VerifierErrors(Vec<VerifierError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
    Void,
    String,
}

impl From<Type> for types::Type {
    fn from(val: Type) -> Self {
        match val {
            Type::Int => types::I64,
            Type::Float => types::F64,
            Type::Void => types::INVALID,
            Type::String => types::I64, // TODO: platform ptr size
        }
    }
}

impl From<Type> for AbiParam {
    fn from(value: Type) -> Self {
        AbiParam::new(value.into())
    }
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "integer"),
            Type::Float => write!(f, "float"),
            Type::Void => write!(f, "void"),
            Type::String => write!(f, "string"),
        }
    }
}

pub fn generate(program: Program) -> Result<ObjectProduct, GeneratorErrors> {
    #[cfg(target_os = "linux")]
    const TARGET_TRIPLE: &str = "x86_64-unknown-linux";

    #[cfg(target_os = "windows")]
    const TARGET_TRIPLE: &str = "x86_64-pc-windows-gnu";

    let isa = {
        let mut builder = settings::builder();

        // disable optimizations so dissassembly will more directly correlated to our Cranelift usage
        builder.set("opt_level", "none").unwrap();

        builder.enable("is_pic").unwrap();

        let flags = settings::Flags::new(builder);

        isa::lookup_by_name(TARGET_TRIPLE)
            .unwrap()
            .finish(flags)
            .unwrap()
    };

    let mut module = {
        let translation_unit_name = b"output_a_binary";
        let libcall_names = cranelift_module::default_libcall_names();
        let builder =
            ObjectBuilder::new(isa.clone(), translation_unit_name, libcall_names).unwrap();
        ObjectModule::new(builder)
    };
    let mut context = Context::new();

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
        .declare_function("main", cranelift_module::Linkage::Export, &main_signature)
        .unwrap();

    let mut function_context = FunctionBuilderContext::new();

    let mut builder = FunctionBuilder::new(&mut context.func, &mut function_context);
    builder.func.signature = main_signature;

    let main_block = builder.create_block();
    builder.switch_to_block(main_block);
    builder.seal_block(main_block);

    let printf_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![AbiParam::new(types::I64)],
        returns: vec![AbiParam::new(types::I32)],
    };

    let printf_id = module
        .declare_function(
            "printf",
            cranelift_module::Linkage::Import,
            &printf_signature,
        )
        .unwrap();

    let generator = Generator {
        isa: isa.clone(),
        builder,
        module: &mut module,
        strings: 0,
        printf_id,
        returned: false,
        current_scope: Scope::new(),
        var_count: 0,
    };

    generator.build(program)?;

    println!("{}", context.func.display());

    let flags = settings::Flags::new(settings::builder());
    if let Err(errs) = verify_function(&context.func, &flags) {
        return Err(GeneratorErrors::VerifierErrors(errs.0));
    }

    module.define_function(main_id, &mut context).unwrap();

    Ok(module.finish())
}

fn get_printf_ref() -> FuncRef {
    todo!()
}

type BuildValueResult = Result<(Value, Type), Vec<Spanned<CodeError>>>;

type BuildStatementResult = Result<(), Vec<Spanned<CodeError>>>;

struct Scope {
    vars: HashMap<String, (Variable, Type)>,
    parent: Option<Box<Scope>>,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            parent: None,
        }
    }

    pub fn get_var(&self, name: &str) -> Option<&(Variable, Type)> {
        self.vars
            .get(name)
            .or_else(|| self.parent.as_ref()?.get_var(name))
    }

    pub fn insert(&mut self, name: String, var: (Variable, Type)) -> Result<(), ()> {
        use std::collections::hash_map::Entry;

        match self.vars.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(var);
                Ok(())
            }
            Entry::Occupied(_) => Err(()),
        }
    }
}

struct Generator<'a> {
    isa: std::sync::Arc<dyn TargetIsa>,
    builder: FunctionBuilder<'a>,
    module: &'a mut ObjectModule,
    strings: usize,
    printf_id: FuncId,
    returned: bool,
    current_scope: Scope,
    var_count: usize,
}

impl Generator<'_> {
    fn build(mut self, program: Program) -> Result<(), GeneratorErrors> {
        let mut errors = Vec::new();
        for statement in &program.statements {
            if let Err(mut error) = self.build_statement(statement) {
                errors.append(&mut error);
            }
        }

        if !self.returned {
            let zero = self.builder.ins().iconst(types::I32, 0);
            self.builder.ins().return_(&[zero]);
        }

        if !errors.is_empty() {
            return Err(GeneratorErrors::CodeErrors(errors));
        }

        Ok(())
    }

    fn build_statement(&mut self, statement: &Spanned<Statement>) -> BuildStatementResult {
        match &statement.0 {
            Statement::Return(expr) => {
                let value = self.build_expression(expr)?;

                if value.1 != Type::Int {
                    return CodeError::InvalidReturnType(value.1).err(expr.1);
                }

                let cast = self.builder.ins().ireduce(types::I32, value.0);
                self.builder.ins().return_(&[cast]);
                self.returned = true;
            }
            Statement::Expression(expr) => {
                self.build_expression(expr)?;
            }
            Statement::Binding(name, expr) => {
                let value = self.build_expression(expr)?;

                let Ok(var) = self.new_var(name.clone(), value.1) else {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                };

                self.builder.declare_var(var, value.1.into());
                self.builder.def_var(var, value.0);
            }
            Statement::Assignment(name, expr) => {
                let value = self.build_expression(expr)?;

                if let Some(&(var, var_type)) = self.current_scope.get_var(name) {
                    if value.1 != var_type {
                        return CodeError::AssignmentIncompatibleTypes(var_type, value.1)
                            .err(statement.1);
                    }

                    self.builder.def_var(var, value.0);
                } else {
                    return CodeError::UnknownVariable(name.clone()).err(statement.1);
                }
            }
            Statement::Block(statements) => {
                let mut errors = Vec::new();

                let new_block = self.builder.create_block();
                self.builder.ins().jump(new_block, []);
                self.builder.switch_to_block(new_block);
                self.builder.seal_block(new_block);
                self.push_scope();

                for statement in statements {
                    if let Err(mut error) = self.build_statement(statement) {
                        errors.append(&mut error);
                    }
                }

                if !self.pop_scope() {
                    unreachable!("Must always be able to pop scope after leaving block.");
                }

                let new_block = self.builder.create_block();
                self.builder.ins().jump(new_block, []);
                self.builder.switch_to_block(new_block);
                self.builder.seal_block(new_block);

                if !errors.is_empty() {
                    return Err(errors);
                }
            }
        }

        Ok(())
    }

    fn build_expression(&mut self, expression: &Spanned<Expression>) -> BuildValueResult {
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
            Expression::Binary(op, a, b) => {
                let a_value = self.build_expression(a.as_ref())?;
                let b_value = self.build_expression(b.as_ref())?;

                if a_value.1 != b_value.1 {
                    return CodeError::IncompatipleBinOpTypes(*op, a_value.1, b_value.1).err(*span);
                }

                let result_type = a_value.1;
                let result_value = match result_type {
                    Type::Int => self.build_int_bin_op(*op, a_value.0, b_value.0),
                    Type::Float => self.build_float_bin_op(*op, a_value.0, b_value.0),
                    _ => {
                        return CodeError::InvalidBinOpType(*op, result_type).err(*span);
                    }
                };

                Ok((result_value, a_value.1))
            }
            Expression::Intrinsic(intrinsic) => match intrinsic {
                Intrinsic::Print(args) => self.build_printf(span, args),
            },
            Expression::Identifier(name) => {
                let Some((var, var_type)) = self.current_scope.get_var(name) else {
                    return CodeError::UnknownVariable(name.to_owned()).err(*span);
                };

                Ok((self.builder.use_var(*var), *var_type))
            }
        }
    }

    fn build_int_bin_op(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        match op {
            BinOp::Add => self.builder.ins().iadd(a, b),
            BinOp::Sub => self.builder.ins().isub(a, b),
            BinOp::Mul => self.builder.ins().imul(a, b),
            BinOp::Div => self.builder.ins().sdiv(a, b),
        }
    }

    fn build_float_bin_op(&mut self, op: BinOp, a: Value, b: Value) -> Value {
        match op {
            BinOp::Add => self.builder.ins().fadd(a, b),
            BinOp::Sub => self.builder.ins().fsub(a, b),
            BinOp::Mul => self.builder.ins().fmul(a, b),
            BinOp::Div => self.builder.ins().fdiv(a, b),
        }
    }

    fn build_printf(
        &mut self,
        span: &SimpleSpan,
        args: &[Spanned<Expression>],
    ) -> BuildValueResult {
        let Some((format_string_arg, other_args)) = args.split_first() else {
            return CodeError::IntrinsicTooFewArguments("print", 1, args.len()).err(*span);
        };

        // TODO: validate format string/generate own format string
        let Expression::String(_) = &format_string_arg.0 else {
            return CodeError::ExpectedStringLiteral.err(format_string_arg.1);
        };

        let arg_values = other_args
            .iter()
            .map(|expr| {
                let result = self.build_expression(expr);
                if let Ok(value) = &result {
                    const PRINT_ARG_TYPES: [Type; 2] = [Type::Int, Type::String];
                    if !PRINT_ARG_TYPES.contains(&value.1) {
                        return CodeError::IntrisicArgumentType("print", &PRINT_ARG_TYPES, value.1)
                            .err(expr.1);
                    }
                }
                result
            })
            .collect_all()
            .map_err(|errors| {
                errors
                    .into_iter()
                    .flatten()
                    .collect::<Vec<Spanned<CodeError>>>()
            })?;

        let format_string_value = self.build_expression(format_string_arg)?;
        debug_assert_eq!(format_string_value.1, Type::String);

        // Because printf is a variadic function we have to change it's signature on the fly
        // See: adjust_call_for_c_variadic() in rustc_codegen_cranelift
        let mut params = Vec::with_capacity(arg_values.len() + 1);
        params.push(AbiParam::new(self.isa.pointer_type()));
        params.extend(arg_values.iter().map(|(_, t)| AbiParam::from(*t)));

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
        let old_scope = std::mem::replace(
            &mut self.current_scope,
            Scope {
                vars: HashMap::new(),
                parent: None,
            },
        );
        self.current_scope.parent = Some(Box::new(old_scope));
    }

    fn pop_scope(&mut self) -> bool {
        if let Some(mut parent) = self.current_scope.parent.take() {
            std::mem::swap(&mut self.current_scope, &mut *parent);
            true
        } else {
            // No parent to pop to — already at root
            false
        }
    }

    fn new_var(&mut self, name: String, var_type: Type) -> Result<Variable, ()> {
        let var = Variable::new(self.var_count);
        self.current_scope.insert(name, (var, var_type))?;
        self.var_count += 1;
        Ok(var)
    }
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}
