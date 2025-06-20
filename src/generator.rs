use std::fmt::Display;

use chumsky::span::SimpleSpan;
use cranelift::{
    codegen::{Context, verifier::VerifierError, verify_function},
    prelude::*,
};
use cranelift_codegen::ir::FuncRef;
use cranelift_module::{DataDescription, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};

use crate::parser::{BinOp, Expression, Intrinsic, Program, Spanned, Statement};

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

    #[error("Intrisic '{}' invoked with too few arguments. Expected {} but found {}.", .0, .1, .2)]
    IntrinsicTooFewArguments(&'static str, usize, usize),
    #[error("Intrisic '{}' invoked with too many arguments. Expected {} but found {}.", .0, .1, .2)]
    IntrinsicTooManyArguments(&'static str, usize, usize),
    #[error("Intrisic '{}' invoked with wrong argument type. Expected one of '{}' but found '{}'.", .0, join(.1), .2)]
    IntrisicArgumentType(&'static str, &'static [Type], Type),
}

impl CodeError {
    fn spanned(self, span: SimpleSpan) -> Spanned<Self> {
        Spanned(self, span)
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

    let printf_signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![AbiParam::new(isa.pointer_type()), AbiParam::new(types::I64)],
        returns: vec![AbiParam::new(types::I32)],
    };

    let printf_id = module
        .declare_function(
            "printf",
            cranelift_module::Linkage::Import,
            &printf_signature,
        )
        .unwrap();

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

    let printf_ref = module.declare_func_in_func(printf_id, builder.func);

    let main_block = builder.create_block();
    builder.switch_to_block(main_block);
    builder.seal_block(main_block);

    let generator = Generator {
        builder,
        module: &mut module,
        strings: 0,
        printf_ref,
        returned: false,
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

type BuildValueResult = Result<(Value, Type), Spanned<CodeError>>;

struct Generator<'a> {
    builder: FunctionBuilder<'a>,
    module: &'a mut ObjectModule,
    strings: usize,
    printf_ref: FuncRef,
    returned: bool,
}

impl Generator<'_> {
    fn build(mut self, program: Program) -> Result<(), GeneratorErrors> {
        let mut errors = Vec::new();
        for statement in &program.statements {
            if let Err(error) = self.build_statement(statement) {
                errors.push(error);
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

    fn build_statement(
        &mut self,
        statement: &Spanned<Statement>,
    ) -> Result<(), Spanned<CodeError>> {
        match &statement.0 {
            Statement::Return(expr) => {
                let value = self.build_expression(expr)?;

                if value.1 != Type::Int {
                    return Err(Spanned(CodeError::InvalidReturnType(value.1), expr.1));
                }

                let cast = self.builder.ins().ireduce(types::I32, value.0);
                self.builder.ins().return_(&[cast]);
                self.returned = true;
            }
            Statement::Expression(expr) => {
                self.build_expression(expr)?;
            }
            _ => todo!("{statement:?}"),
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
                    .declare_data(
                        &format!("format_string_{string_id}"),
                        Linkage::Local,
                        true,
                        false,
                    )
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
                    return Err(Spanned(
                        CodeError::IncompatipleBinOpTypes(*op, a_value.1, b_value.1),
                        *span,
                    ));
                }

                let result_type = a_value.1;
                let result_value = match result_type {
                    Type::Int => self.build_int_bin_op(*op, a_value.0, b_value.0),
                    Type::Float => self.build_float_bin_op(*op, a_value.0, b_value.0),
                    _ => {
                        return Err(CodeError::InvalidBinOpType(*op, result_type).spanned(*span));
                    }
                };

                Ok((result_value, a_value.1))
            }
            Expression::Intrinsic(intrinsic) => match intrinsic {
                Intrinsic::Print(args) => {
                    const ARG_COUNT: usize = 2;
                    if args.len() < ARG_COUNT {
                        return Err(CodeError::IntrinsicTooFewArguments(
                            intrinsic.name(),
                            ARG_COUNT,
                            args.len(),
                        )
                        .spanned(*span));
                    } else if args.len() > ARG_COUNT {
                        return Err(CodeError::IntrinsicTooManyArguments(
                            intrinsic.name(),
                            ARG_COUNT,
                            args.len(),
                        )
                        .spanned(*span));
                    }

                    let arg_a = &args[0];
                    let arg_a_value = self.build_expression(arg_a)?;

                    if arg_a_value.1 != Type::String {
                        return Err(CodeError::IntrisicArgumentType(
                            intrinsic.name(),
                            &[Type::String],
                            arg_a_value.1,
                        )
                        .spanned(arg_a.1));
                    }

                    let arg_b = &args[1];
                    let arg_b_value = self.build_expression(arg_b)?;

                    const PRINT_ARG_TYPES: [Type; 3] = [Type::Int, Type::Float, Type::String];
                    if !PRINT_ARG_TYPES.contains(&arg_b_value.1) {
                        return Err(CodeError::IntrisicArgumentType(
                            intrinsic.name(),
                            &PRINT_ARG_TYPES,
                            arg_b_value.1,
                        )
                        .spanned(arg_b.1));
                    }

                    // TODO: use platform ptr size?
                    let result = self
                        .builder
                        .ins()
                        .call(self.printf_ref, &[arg_a_value.0, arg_b_value.0]);
                    let value = self.builder.func.dfg.inst_results(result)[0];
                    // TODO: Type::I32
                    Ok((value, Type::Int))
                }
            },
            _ => Err(Spanned(
                CodeError::NoYetSupported(format!("{expression:?}",)),
                *span,
            )),
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
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}
