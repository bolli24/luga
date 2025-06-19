use std::fmt::Display;

use cranelift::{
    codegen::{Context, verifier::VerifierError, verify_function},
    prelude::*,
};
use cranelift_module::Module;
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};

use crate::parser::{BinOp, Expression, Program, Spanned, Statement};

#[derive(Debug, Clone, thiserror::Error)]
pub enum CodeError {
    #[error("Return value type must be integer, found: {}", .0)]
    InvalidReturnType(Type),

    #[error("{} isn't supported yet.", .0)]
    NoYetSupported(String),

    #[error("Incompatiple types for binary operator '{}': '{}' and '{}'", .0, .1, .2)]
    IncompatipleBinOpTypes(BinOp, Type, Type),
}

pub enum GeneratorErrors {
    CodeErrors(Vec<Spanned<CodeError>>),
    VerifierErrors(Vec<VerifierError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "integer"),
            Type::Float => write!(f, "float"),
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
        builder.set("opt_level", "speed").unwrap();

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

    let signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![],
        // Since we're linking to libc, we can return the exit code from main.
        returns: vec![AbiParam::new(types::I32)],
    };

    let main_id = module
        .declare_function("main", cranelift_module::Linkage::Export, &signature)
        .unwrap();

    // let mut vars: IndexMap<String, Variable> = IndexMap::new();

    let mut returned = false;

    let mut function_context = FunctionBuilderContext::new();

    let mut builder = FunctionBuilder::new(&mut context.func, &mut function_context);
    builder.func.signature = signature;

    let main_block = builder.create_block();
    builder.switch_to_block(main_block);
    builder.seal_block(main_block);

    let mut errors = Vec::new();

    for statement in &program.statements {
        if let Err(error) = build_statement(&mut builder, &mut returned, statement) {
            errors.push(error);
        }
    }

    if !returned {
        let zero = builder.ins().iconst(types::I32, 0);
        builder.ins().return_(&[zero]);
    }

    if !errors.is_empty() {
        return Err(GeneratorErrors::CodeErrors(errors));
    }

    builder.finalize();

    println!("{}", context.func.display());

    let flags = settings::Flags::new(settings::builder());
    if let Err(errs) = verify_function(&context.func, &flags) {
        return Err(GeneratorErrors::VerifierErrors(errs.0));
    }

    module.define_function(main_id, &mut context).unwrap();

    Ok(module.finish())
}

fn build_statement(
    builder: &mut FunctionBuilder<'_>,
    returned: &mut bool,
    statement: &Spanned<Statement>,
) -> Result<(), Spanned<CodeError>> {
    match &statement.0 {
        Statement::Return(expr) => {
            let value = build_expression(builder, expr)?;

            if value.1 != Type::Int {
                return Err(Spanned(CodeError::InvalidReturnType(value.1), expr.1));
            }

            let cast = builder.ins().ireduce(types::I32, value.0);
            builder.ins().return_(&[cast]);
            *returned = true;
        }
        Statement::Expression(expr) => {
            build_expression(builder, expr)?;
        }
        _ => todo!("{statement:?}"),
    }

    Ok(())
}

type BuildValueResult = Result<(Value, Type), Spanned<CodeError>>;

fn build_expression(
    builder: &mut FunctionBuilder<'_>,
    expression: &Spanned<Expression>,
) -> BuildValueResult {
    let Spanned(expression, span) = expression;
    match expression {
        Expression::Float(value) => Ok((builder.ins().f64const(*value), Type::Float)),
        Expression::Integer(value) => Ok((builder.ins().iconst(types::I64, *value), Type::Int)),
        Expression::Binary(op, a, b) => {
            let a_value = build_expression(builder, a.as_ref())?;
            let b_value = build_expression(builder, b.as_ref())?;

            if a_value.1 != b_value.1 {
                return Err(Spanned(
                    CodeError::IncompatipleBinOpTypes(*op, a_value.1, b_value.1),
                    *span,
                ));
            }

            let result_type = a_value.1;
            let result_value = match result_type {
                Type::Int => build_int_bin_op(builder, *op, a_value.0, b_value.0),
                Type::Float => build_float_bin_op(builder, *op, a_value.0, b_value.0),
            };

            Ok((result_value, a_value.1))
        }
        _ => Err(Spanned(
            CodeError::NoYetSupported(format!("{expression:?}",)),
            *span,
        )),
    }
}

fn build_int_bin_op(builder: &mut FunctionBuilder<'_>, op: BinOp, a: Value, b: Value) -> Value {
    match op {
        BinOp::Add => builder.ins().iadd(a, b),
        BinOp::Sub => builder.ins().isub(a, b),
        BinOp::Mul => builder.ins().imul(a, b),
        BinOp::Div => builder.ins().sdiv(a, b),
    }
}

fn build_float_bin_op(builder: &mut FunctionBuilder<'_>, op: BinOp, a: Value, b: Value) -> Value {
    match op {
        BinOp::Add => builder.ins().fadd(a, b),
        BinOp::Sub => builder.ins().fsub(a, b),
        BinOp::Mul => builder.ins().fmul(a, b),
        BinOp::Div => builder.ins().fdiv(a, b),
    }
}
