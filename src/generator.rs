use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use chumsky::span::SimpleSpan;
use cranelift::{
    codegen::{verifier::VerifierError, verify_function, Context},
    prelude::*,
};
use cranelift_codegen::isa::TargetIsa;
use cranelift_module::{DataDescription, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};

use crate::{
    parser::{BinOp, Expression, Intrinsic, Program, Spanned, Statement},
    util::ResultCollectAll,
};

const UNREACHABLE: TrapCode = TrapCode::unwrap_user(100);

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

    fn spanned(self, span: SimpleSpan) -> Spanned<Self> {
        Spanned(self, span)
    }
}

pub enum GeneratorErrors {
    CodeErrors(ManyErrors),
    VerifierErrors(Vec<VerifierError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
    Void,
    Never,
    String,
    Bool,
}

impl From<Type> for types::Type {
    fn from(val: Type) -> Self {
        match val {
            Type::Int => types::I64,
            Type::Float => types::F64,
            Type::Void => types::INVALID,
            Type::Never => types::INVALID,
            Type::String => types::I64, // TODO: platform ptr size
            Type::Bool => types::I8,
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
            Type::Never => write!(f, "never"),
            Type::String => write!(f, "string"),
            Type::Bool => write!(f, "bool"),
        }
    }
}

pub fn generate(program: Program, verbose: bool) -> Result<ObjectProduct, GeneratorErrors> {
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

    let exit_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![AbiParam::new(types::I32)],
        returns: vec![],
    };

    let exit_id = module
        .declare_function("exit", cranelift_module::Linkage::Import, &exit_signature)
        .unwrap();

    let generator = Generator {
        isa: isa.clone(),
        builder,
        module: &mut module,
        strings: 0,
        printf_id,
        exit_id,
        returned: false,
        current_scope: Scope::new(),
        var_count: 0,
    };

    generator.build(program)?;

    if verbose {
        println!("{}", context.func.display());
    }

    let flags = settings::Flags::new(settings::builder());
    if let Err(errs) = verify_function(&context.func, &flags) {
        return Err(GeneratorErrors::VerifierErrors(errs.0));
    }

    module.define_function(main_id, &mut context).unwrap();

    Ok(module.finish())
}

type ManyErrors = Vec<Spanned<CodeError>>;

struct Scope {
    vars_name: HashMap<String, (Variable, Type)>,
    vars: HashSet<usize>,
    parent: Option<Box<Scope>>,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            vars_name: HashMap::new(),
            vars: HashSet::new(),
            parent: None,
        }
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
}

struct Generator<'a> {
    isa: std::sync::Arc<dyn TargetIsa>,
    builder: FunctionBuilder<'a>,
    module: &'a mut ObjectModule,
    strings: usize,
    printf_id: FuncId,
    exit_id: FuncId,
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

    fn build_statement(&mut self, statement: &Spanned<Statement>) -> Result<(), ManyErrors> {
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

                let Ok(var) = self.new_var(name.clone(), value) else {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                };

                if !matches!(value.1, Type::Int | Type::Float | Type::String | Type::Bool) {
                    return CodeError::BindingType(value.1).err(expr.1);
                }

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

                if self.pop_scope().is_none() {
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
            Statement::If(condition, then_block, else_block) => {
                // build_if_example(&mut self.builder);
                self.build_if(condition, then_block, else_block)?;
            }
        }

        Ok(())
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
                self.builder.ins().iconst(types::I8, *value as i64),
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
                .ok_or(CodeError::InvalidBinOpType(*op, x_value.1).spanned(*span))?;
                Ok(result)
            }
            Expression::Intrinsic(intrinsic) => match intrinsic {
                Intrinsic::Print(args) => self.build_printf(span, args),
                Intrinsic::Assert(args) => self.build_assert(span, args),
                Intrinsic::Exit(args) => self.build_exit(span, args),
            },
            Expression::Identifier(name) => {
                let Some((var, var_type)) = self.current_scope.get_var(name) else {
                    return CodeError::UnknownVariable(name.to_owned()).err(*span);
                };

                Ok((self.builder.use_var(*var), *var_type))
            }
        }
    }

    fn build_int_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some((
            match op {
                BinOp::Add => self.builder.ins().iadd(x, y),
                BinOp::Sub => self.builder.ins().isub(x, y),
                BinOp::Mul => self.builder.ins().imul(x, y),
                BinOp::Div => self.builder.ins().sdiv(x, y),
                BinOp::Equal => self.builder.ins().icmp(IntCC::Equal, x, y),
            },
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Type::Int,
                BinOp::Equal => Type::Bool,
            },
        ))
    }

    fn build_float_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some((
            match op {
                BinOp::Add => self.builder.ins().fadd(x, y),
                BinOp::Sub => self.builder.ins().fsub(x, y),
                BinOp::Mul => self.builder.ins().fmul(x, y),
                BinOp::Div => self.builder.ins().fdiv(x, y),
                BinOp::Equal => self.builder.ins().fcmp(FloatCC::Equal, x, y),
            },
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => Type::Int,
                BinOp::Equal => Type::Bool,
            },
        ))
    }

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
    ) -> Result<(Value, Type), ManyErrors> {
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
            .map_err(|errors| errors.into_iter().flatten().collect::<ManyErrors>())?;

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
    fn build_if(
        &mut self,
        condition: &Spanned<Expression>,
        then_body: &[Spanned<Statement>],
        else_body: &[Spanned<Statement>],
    ) -> Result<(), ManyErrors> {
        let condition_block = self.builder.create_block();
        self.builder.ins().jump(condition_block, []);
        self.builder.switch_to_block(condition_block);
        self.builder.seal_block(condition_block);

        let condition_value = self.build_expression(condition)?;

        if condition_value.1 != Type::Bool {
            return CodeError::InvalidConditionType(condition_value.1).err(condition.1);
        }

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(condition_value.0, then_block, [], else_block, []);

        self.builder.seal_block(then_block);
        self.builder.seal_block(else_block);

        self.builder.switch_to_block(then_block);
        let then_value = self.build_block_body(then_body)?;
        self.builder.ins().jump(exit_block, []);

        self.builder.switch_to_block(else_block);
        let else_value = self.build_block_body(else_body)?;
        self.builder.ins().jump(exit_block, []);

        self.builder.switch_to_block(exit_block);

        if then_value.1 != else_value.1 {
            return CodeError::IfExpressionType
                .err(else_body.last().or(then_body.last()).unwrap().1);
        }

        Ok(())
    }

    fn build_block_body(
        &mut self,
        then_body: &[Spanned<Statement>],
    ) -> Result<(Value, Type), ManyErrors> {
        self.push_scope();

        let mut errors = Vec::new();

        for statement in then_body {
            if let Err(mut error) = self.build_statement(statement) {
                errors.append(&mut error);
            }
        }

        let Some(_) = self.pop_scope() else {
            unreachable!("Must always be able to pop scope after leaving block.");
        };

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok((Value::new((u32::MAX - 1) as usize), Type::Void))
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
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}
