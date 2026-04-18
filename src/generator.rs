use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use chumsky::span::{SimpleSpan, Span};
use cranelift::{
    codegen::{Context, ir::ArgumentPurpose, verifier::VerifierError, verify_function},
    module::{DataDescription, FuncId, Linkage, Module, default_libcall_names},
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{isa::CallConv, *},
};
use indexmap::IndexMap;

use crate::{
    parser::{
        BinOp, Conditional, Expression, FunctionDecl, If, Intrinsic, Param, Program, Spanned,
        Statement, StructDecl, UnOp,
    },
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
    #[error("Invalid type '{}' for unary operator '{}'", .1 ,.0)]
    InvalidUnOpType(UnOp, Type),

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
    #[error("Type '{}' already declared.", .0)]
    TypeRedeclaration(String),
    #[error("Can't coerse integer literal of value '{}' into integer type '{}'", .0, .0)]
    CoerseInt(Type, i64),

    #[error("Duplicate function parameter '{}'", .0)]
    FunctionParamDuplicate(String),
    #[error("Function '{}' already declared.", .0)]
    FunctionRedeclaration(String),
    #[error("Call to unknown function '{}'.", .0)]
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

    #[error("Must declare function in top level scope.")]
    StructDeclNotTopLevel,
    #[error("Struct field '{}' already declared.", .0)]
    StructDuplicateField(String),
    #[error("Type '{}' has no field named '{}'.", .0, .1)]
    UnknownField(Type, String),
    #[error("Type '{}' is not a struct type.", .0)]
    NotStructType(Type),
    #[error("Duplicate initialization of field '{}'.", .0)]
    StructInitArgDuplicate(String),
    #[error("Missing initialization of field '{}'.", .0)]
    StructInitMissingArg(String),
    #[error("Invalid type for field initialization. Expected '{}' but found '{}'.", .0, .1)]
    StructInitInvalidType(Type, Type),
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
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Void,
    Never,
    String,
    Bool,
    Fn(FuncType),
    Struct(StructType),
    // TODO: Named(String),
}

impl Type {
    fn size(&self) -> u32 {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 => 4,
            Self::I64 | Self::U64 => 8,
            Self::F32 => 4,
            Self::F64 => 8,
            Self::Void | Self::Never => 0,
            Self::String => 8, // TODO: platform pointer size
            Self::Bool => 1,
            Self::Fn(_) => 0,
            Self::Struct(struct_type) => struct_type.size(),
        }
    }

    fn align_shift(&self) -> u8 {
        match self {
            Self::I8 | Self::U8 => 0,
            Self::I16 | Self::U16 => 1,
            Self::I32 | Self::U32 => 2,
            Self::I64 | Self::U64 => 3,
            Self::F32 => 2,
            Self::F64 => 3,
            Self::Void | Self::Never => 0,
            Self::String => 3,
            Self::Bool => 0,
            Self::Fn(_) => 0,
            Self::Struct(struct_type) => struct_type.align_shift(),
        }
    }

    fn struct_type(&self) -> Option<&StructType> {
        if let Type::Struct(struct_type) = self {
            Some(struct_type)
        } else {
            None
        }
    }

    fn is_int(&self) -> bool {
        matches!(
            self,
            Self::I8
                | Self::U8
                | Self::I16
                | Self::U16
                | Self::I32
                | Self::U32
                | Self::I64
                | Self::U64
        )
    }

    fn is_signed_int(&self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    fn is_unsigned_int(&self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    name: String,
    params: Vec<Type>,
    return_type: Box<Type>,
    is_extern: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructType {
    name: String,
    fields: IndexMap<String, StructField>,
}

impl StructType {
    fn size(&self) -> u32 {
        let mut offset = 0;
        for field in self.fields.values() {
            let align = 1 << field.r#type.align_shift();
            let padding = (align - (offset % align)) % align;
            offset += padding;
            offset += field.r#type.size();
        }

        // Final padding so total size is a multiple of the struct's alignment
        let struct_align = 1 << self.align_shift();
        let padding = (struct_align - (offset % struct_align)) % struct_align;
        offset + padding
    }

    fn size_pad_8(&self) -> u32 {
        let size = self.size();
        if size % 8 == 0 {
            size
        } else {
            size + (8 - size % 8)
        }
    }

    fn align_shift(&self) -> u8 {
        self.fields
            .values()
            .map(|f| f.r#type.align_shift())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    r#type: Type,
    offset: i32,
}

impl Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::I8 => write!(f, "i8"),
            Self::I16 => write!(f, "i16"),
            Self::I32 => write!(f, "i32"),
            Self::I64 => write!(f, "i64"),
            Self::U8 => write!(f, "u8"),
            Self::U16 => write!(f, "u16"),
            Self::U32 => write!(f, "u32"),
            Self::U64 => write!(f, "u64"),
            Self::F32 => write!(f, "f32"),
            Self::F64 => write!(f, "f64"),
            Self::Void => write!(f, "void"),
            Self::Never => write!(f, "never"),
            Self::String => write!(f, "string"),
            Self::Bool => write!(f, "bool"),
            Self::Fn(func_type) => write!(f, "{func_type}"),
            Self::Struct(struct_type) => write!(f, "{struct_type}"),
        }
    }
}

impl Display for FuncType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}fn {} ({}) : {}",
            if self.is_extern { "extern " } else { "" },
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

impl Display for StructType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "struct {} {{ {} }}",
            self.name,
            self.fields
                .iter()
                .map(|field| { format!("{}@{}: {}", field.0, field.1.offset, field.1.r#type) })
                .collect::<Vec<_>>()
                .join(", "),
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
            return_type: Type::I32.into(), // TODO: I32
            is_extern: false,
        },
        return_dest: None,
        new_types: HashMap::new(),
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
    return_dest: Option<Variable>,
    new_types: HashMap<String, StructType>,
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

    /// Returns Ok(true) if all paths return from the function
    fn build_statement(&mut self, statement: &Spanned<Statement>) -> Result<bool, ManyErrors> {
        match &statement.0 {
            Statement::Return(expr) => {
                return self.build_return(expr);
            }
            Statement::Expression(expr) => {
                self.build_expression(expr, None)?;
            }
            Statement::Binding(name, expr) => {
                let value = self.build_expression(expr, None)?;

                if self.current_scope.vars_name.contains_key(name) {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                };

                let Some(var_type) = self.get_crane_type(&value.1) else {
                    return CodeError::BindingType(value.1).err(expr.1);
                };

                let var = self.builder.declare_var(var_type);
                self.builder.def_var(var, value.0);
                self.new_var(name.to_owned(), var, value);
            }
            Statement::Assignment(name, expr) => {
                if let Some((var, var_type)) = self.current_scope.get_var(name).cloned() {
                    let value = self.build_expression(expr, Some(var_type.clone()))?;

                    if value.1 != var_type {
                        return CodeError::AssignmentIncompatibleTypes(var_type.clone(), value.1)
                            .err(statement.1);
                    }

                    self.builder.def_var(var, value.0);
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
            Statement::While(r#while) => return self.build_while(r#while),
            Statement::FunctionDecl(decl) => {
                return self.build_fn_decl(decl, &statement.1).map(|_| false);
            }
            Statement::StructDecl(decl) => {
                return self.build_struct_decl(decl, &statement.1).map(|_| false);
            }
        }

        Ok(false)
    }

    fn build_return_field_copy(
        &mut self,
        struct_field: &StructField,
        current_offset: &mut i32,
        struct_addr: Value,
        return_dest: Value,
    ) {
        // Align current offset to field's alignment requirement
        let field_align_shift = struct_field.r#type.align_shift();
        *current_offset = Self::align_to(*current_offset, field_align_shift);

        match &struct_field.r#type {
            Type::Struct(struct_type) => {
                // For nested structs, recursively copy each field
                for field in struct_type.fields.values() {
                    self.build_return_field_copy(field, current_offset, struct_addr, return_dest);
                }
            }
            _ => {
                // Copy primitive field
                let crane_type = self
                    .get_crane_type(&struct_field.r#type)
                    .expect("Field type must be valid");
                let value = self.builder.ins().load(
                    crane_type,
                    MemFlags::trusted(),
                    struct_addr,
                    *current_offset,
                );
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, return_dest, *current_offset);

                // Advance offset by field size
                *current_offset += struct_field.r#type.size() as i32;
            }
        }
    }

    // Helper function for alignment
    fn align_to(offset: i32, align_shift: u8) -> i32 {
        let align = 1 << align_shift;
        (offset + align - 1) & !(align - 1)
    }

    fn build_return(&mut self, expr: &Spanned<Expression>) -> Result<bool, ManyErrors> {
        let value =
            self.build_expression(expr, Some(*self.current_func_type.return_type.clone()))?;
        if self.current_func_type.name == "main" {
            if value.1 != Type::Never && value.1 != Type::I32 {
                // self.builder.ins().return_(&[]);
                return CodeError::InvalidReturnType(Type::I32, value.1).err(expr.1);
            }
            if value.1 != Type::Never {
                self.builder.ins().return_(&[value.0]);
            }
        } else {
            if value.1 != Type::Never && *self.current_func_type.return_type != value.1 {
                return CodeError::InvalidReturnType(
                    (*self.current_func_type.return_type).clone(),
                    value.1.clone(),
                )
                .err(expr.1);
            }
            match value.1 {
                // Diverging expression already terminated the block — nothing to emit.
                Type::Never => {}
                Type::Void => {
                    self.builder.ins().return_(&[]);
                }
                Type::Struct(struct_type) => {
                    let return_dest = self
                        .return_dest
                        .expect("Return destination must be valid when returning struct.");

                    let return_dest = self.builder.use_var(return_dest);

                    let mut current_offset = 0;

                    for field in struct_type.fields.values() {
                        self.build_return_field_copy(
                            field,
                            &mut current_offset,
                            value.0,
                            return_dest,
                        );
                    }

                    self.builder.ins().return_(&[]);
                }
                _ => {
                    self.builder.ins().return_(&[value.0]);
                }
            };
        }
        Ok(true)
    }

    fn build_expression(
        &mut self,
        expression: &Spanned<Expression>,
        try_into_type: Option<Type>,
    ) -> Result<(Value, Type), ManyErrors> {
        let Spanned(expression, span) = expression;
        match expression {
            Expression::Float(value) => Ok((self.builder.ins().f32const(*value as f32), Type::F32)),
            Expression::Integer(value) => {
                let into_type = try_into_type.unwrap_or(Type::I32);

                let mut check_and_emit = |min: i64, max: i64, cranelift_type, out_type: Type| {
                    if *value < min || *value > max {
                        CodeError::CoerseInt(out_type.clone(), *value).err(*span)
                    } else {
                        Ok((self.builder.ins().iconst(cranelift_type, *value), out_type))
                    }
                };

                match &into_type {
                    Type::I8 => check_and_emit(i8::MIN as i64, i8::MAX as i64, types::I8, Type::I8),
                    Type::U8 => check_and_emit(u8::MIN as i64, u8::MAX as i64, types::I8, Type::U8),
                    Type::I16 => {
                        check_and_emit(i16::MIN as i64, i16::MAX as i64, types::I16, Type::I16)
                    }
                    Type::U16 => {
                        check_and_emit(u16::MIN as i64, u16::MAX as i64, types::I16, Type::U16)
                    }
                    Type::I32 => {
                        check_and_emit(i32::MIN as i64, i32::MAX as i64, types::I32, Type::I32)
                    }
                    Type::U32 => {
                        check_and_emit(u32::MIN as i64, u32::MAX as i64, types::I32, Type::U32)
                    }
                    Type::I64 => Ok((self.builder.ins().iconst(types::I64, *value), Type::I64)),
                    Type::U64 => {
                        if *value < 0 {
                            CodeError::CoerseInt(Type::U64, *value).err(*span)
                        } else {
                            Ok((self.builder.ins().iconst(types::I64, *value), Type::U64))
                        }
                    }
                    _ => unreachable!("Can't coerce into type {into_type}"),
                }
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
                let x_value = self.build_expression(x.as_ref(), try_into_type.clone())?;
                let y_value = self.build_expression(y.as_ref(), try_into_type)?;

                if x_value.1 != y_value.1 {
                    return CodeError::IncompatipleBinOpTypes(*op, x_value.1, y_value.1).err(*span);
                }

                let result = match x_value.1 {
                    Type::I8 | Type::I32 | Type::I64 => {
                        self.build_int_bin_op(*op, x_value.0, y_value.0, x_value.1)
                    }
                    Type::F32 | Type::F64 => {
                        self.build_float_bin_op(*op, x_value.0, y_value.0, x_value.1)
                    }
                    Type::Bool => self.build_bool_bin_op(*op, x_value.0, y_value.0),
                    _ => None,
                }
                .ok_or_else(|| CodeError::InvalidBinOpType(*op, y_value.1).spanned(*span))?;
                Ok(result)
            }
            Expression::Unary(op, expr) => {
                let value = self.build_expression(expr.as_ref(), try_into_type)?;
                if value.1 == Type::Bool && *op == UnOp::Not {
                    let result = self.builder.ins().bxor_imm(value.0, 1);
                    Ok((result, Type::Bool))
                } else {
                    CodeError::InvalidUnOpType(*op, value.1).err(*span)
                }
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
            Expression::FieldAccess(expr, name) => {
                let expr_value = self.build_expression(expr, None)?;
                let field = match &expr_value.1 {
                    Type::Struct(struct_type) if struct_type.fields.contains_key(&name.0) => {
                        &struct_type.fields[&name.0]
                    }
                    _ => {
                        return CodeError::UnknownField(expr_value.1, name.0.clone()).err(name.1);
                    }
                };

                let field_crane_type = self
                    .get_crane_type(&field.r#type)
                    .expect("Field type must be valid.");

                let struct_value = self.builder.ins().load(
                    field_crane_type,
                    MemFlags::new(),
                    expr_value.0,
                    field.offset,
                );

                Ok((struct_value, field.r#type.clone()))
            }
            Expression::StructInit(struct_init) => {
                let mut errors = Vec::new();
                let mut arg_values = HashMap::with_capacity(struct_init.args.len());

                let r#type = self.type_from_name(&struct_init.name.0);

                for arg in &struct_init.args {
                    let maybe_type = r#type
                        .as_ref()
                        .and_then(|r#type| r#type.struct_type())
                        .and_then(|struct_type| struct_type.fields.get(&arg.0.0).cloned())
                        .map(|struct_field| struct_field.r#type);

                    match self.build_expression(&arg.1, maybe_type) {
                        Ok(value) => {
                            if arg_values.insert(arg.0.0.clone(), value).is_some() {
                                errors.push(
                                    CodeError::StructInitArgDuplicate(arg.0.0.clone())
                                        .spanned(combine_spans(arg.0.1, arg.1.1)),
                                );
                            }
                        }
                        Err(mut errs) => errors.append(&mut errs),
                    }
                }

                let Some(r#type) = self.type_from_name(&struct_init.name.0) else {
                    errors.push(
                        CodeError::UnknownType(struct_init.name.0.clone())
                            .spanned(struct_init.name.1),
                    );
                    return Err(errors);
                };

                let Type::Struct(struct_type) = &r#type else {
                    errors.push(CodeError::NotStructType(r#type).spanned(struct_init.name.1));
                    return Err(errors);
                };

                if !errors.is_empty() {
                    return Err(errors);
                }

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    struct_type.size(),
                    struct_type.align_shift(),
                ));

                let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);

                for (name, field) in &struct_type.fields {
                    let Some(value) = arg_values.get(name) else {
                        errors.push(CodeError::StructInitMissingArg(name.clone()).spanned(*span));
                        continue;
                    };

                    if value.1 != field.r#type {
                        errors.push(
                            CodeError::StructInitInvalidType(field.r#type.clone(), value.1.clone())
                                .spanned(*span),
                        );
                        continue;
                    }

                    self.builder.ins().store(
                        MemFlags::trusted(),
                        value.0,
                        struct_addr,
                        field.offset,
                    );
                }

                if !errors.is_empty() {
                    return Err(errors);
                }

                Ok((struct_addr, r#type))
            }
        }
    }

    #[must_use]
    fn build_int_bin_op(
        &mut self,
        op: BinOp,
        x: Value,
        y: Value,
        r#type: Type,
    ) -> Option<(Value, Type)> {
        if matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte) && r#type.is_unsigned_int()
        {
            todo!("Comparision of unsigned integer types.")
        };

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
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => r#type,
                BinOp::Equal | BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte => Type::Bool,
            },
        ))
    }

    #[must_use]
    fn build_float_bin_op(
        &mut self,
        op: BinOp,
        x: Value,
        y: Value,
        r#type: Type,
    ) -> Option<(Value, Type)> {
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
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => r#type,
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
                let result = self.build_expression(expr, None);
                if let Ok(value) = &result {
                    const PRINT_ARG_TYPES: [Type; 10] = [
                        Type::U8,
                        Type::I8,
                        Type::U16,
                        Type::I16,
                        Type::U32,
                        Type::I32,
                        Type::U64,
                        Type::I64,
                        Type::Bool,
                        Type::String,
                    ];
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

        let format_string_value = self.build_expression(&format_string_arg, None)?;
        debug_assert_eq!(format_string_value.1, Type::String);

        // Because printf is a variadic function we have to change it's signature on the fly
        // See: adjust_call_for_c_variadic() in rustc_codegen_cranelift
        let mut params = Vec::with_capacity(arg_values.len() + 1);
        params.push(AbiParam::new(self.isa.pointer_type()));
        params.extend(arg_values.iter().map(|(_, t)| {
            self.get_extern_abi_param(t)
                .expect("Type must be valid @print parameter.")
        }));

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
        Ok((value, Type::I32))
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

    fn new_var(&mut self, name: String, var: Variable, value: (Value, Type)) {
        self.current_scope
            .insert(name, var, value)
            .expect("Variable must not exist.");
        self.var_count += 1;
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

        let mut returned = else_block.is_some();

        for (index, conditional) in if_statement.if_branches.iter().enumerate() {
            let condition_block = condition_blocks[index];
            let then_block = self.builder.create_block();
            self.builder.seal_block(condition_block);
            self.builder.switch_to_block(condition_block);
            let condition_value = match self.build_expression(&conditional.condition, None) {
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

    fn build_while(&mut self, conditional: &Conditional) -> Result<bool, Vec<Spanned<CodeError>>> {
        let Conditional { condition, block } = conditional;

        let while_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        let condition_block = self.builder.create_block();
        self.builder.ins().jump(condition_block, []);
        self.builder.switch_to_block(condition_block);

        let condition_value = self.build_expression(condition, None)?;
        if condition_value.1 != Type::Bool {
            return CodeError::InvalidConditionType(condition_value.1.clone()).err(condition.1);
        }

        self.builder
            .ins()
            .brif(condition_value.0, while_block, [], exit_block, []);

        self.builder.switch_to_block(while_block);

        self.build_block_body(block)?;

        self.builder.ins().jump(condition_block, []);

        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(condition_block);
        self.builder.seal_block(while_block);
        self.builder.seal_block(exit_block);

        Ok(false)
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

        let condition = self.build_expression(&args[0], None)?;
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

        let expr = self.build_expression(&args[0], Some(Type::I32))?;
        if expr.1 != Type::I32 {
            return CodeError::IntrisicArgumentType("exit", &[Type::I32], expr.1).err(args[0].1);
        }

        let exit_ref = self
            .module
            .declare_func_in_func(self.exit_id, self.builder.func);

        self.builder.ins().call(exit_ref, &[expr.0]);
        Ok((Value::from_u32(u32::MAX - 1), Type::Never))
    }

    fn build_fn_decl(&mut self, decl: &FunctionDecl, span: &SimpleSpan) -> Result<(), ManyErrors> {
        let &FunctionDecl {
            name,
            r#extern: is_extern,
            params,
            body,
            return_type,
        } = &decl;

        if *is_extern {
            assert!(body.is_empty());
        }

        let mut fn_params: Vec<(String, Type, ArgumentPurpose)> = Vec::new();
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

            if fn_params
                .iter()
                .any(|(other_name, _, _)| &name.0 == other_name)
            {
                errors.push(CodeError::FunctionParamDuplicate(name.0.clone()).spanned(name.1));
            }
            param_types.push(r#type.clone());
            let purpose = if let Type::Struct(struct_type) = &r#type
                && *is_extern
            {
                ArgumentPurpose::StructArgument(struct_type.size_pad_8())
            } else {
                ArgumentPurpose::Normal
            };
            fn_params.push((name.0.clone(), r#type, purpose));
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

        if return_type.size() > 8 {
            todo!("Return values larger than 8 bytes.")
        }

        let returns = match &return_type {
            Type::Void | Type::Never => Vec::new(),
            Type::Struct(_) => {
                fn_params.push((
                    "@return_addr".to_owned(),
                    return_type.clone(),
                    ArgumentPurpose::StructReturn,
                ));
                Vec::new()
            }
            return_type => {
                vec![
                    self.get_extern_abi_param(return_type)
                        .expect("Return type must be valid."),
                ]
            }
        };

        let func_type = FuncType {
            name: name.to_owned(),
            params: param_types,
            return_type: return_type.into(),
            is_extern: *is_extern,
        };

        if self.verbose {
            println!("{func_type}");
        }

        let signature = Signature {
            call_conv: self.isa.default_call_conv(),
            params: fn_params
                .iter()
                .map(|(_, r#type, purpose)| {
                    let mut abi_param: AbiParam = if *is_extern {
                        self.get_extern_abi_param(r#type)
                    } else {
                        self.get_abi_param(r#type)
                    }
                    .expect("Param type must be valid.");
                    abi_param.purpose = *purpose;
                    abi_param
                })
                .collect(),
            returns,
        };

        let linkage = if *is_extern {
            Linkage::Import
        } else {
            Linkage::Local
        };

        let func_id = self
            .module
            .declare_function(name, linkage, &signature)
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

        if !*is_extern {
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
                new_types: self.new_types.clone(),
                return_dest: None,
            };

            for (i, (param_name, param_type, purpose)) in fn_params.iter().enumerate() {
                let param_value = fn_generator.builder.block_params(entry_block)[i];

                let var_type = if *is_extern {
                    fn_generator.get_extern_abi_param(param_type)
                } else {
                    fn_generator.get_abi_param(param_type)
                }
                .expect("Parameter type should be valid");

                let var = fn_generator.builder.declare_var(var_type.value_type);
                fn_generator.builder.def_var(var, param_value);
                fn_generator.new_var(param_name.clone(), var, (param_value, param_type.clone()));

                if *purpose == ArgumentPurpose::StructReturn {
                    debug_assert!(fn_generator.return_dest.is_none());
                    fn_generator.return_dest = Some(var);
                }
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

            if self.verbose {
                println!("{}", fn_generator.builder.func.display());
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        if !*is_extern {
            self.module
                .define_function(func_id, &mut fn_context)
                .unwrap();
        }

        Ok(())
    }

    fn type_from_name(&self, type_name: &str) -> Option<Type> {
        Some(match type_name {
            "i8" => Type::I8,
            "i16" => Type::I16,
            "i32" => Type::I32,
            "i64" => Type::I64,
            "u8" => Type::U8,
            "u16" => Type::U16,
            "u32" => Type::U32,
            "u64" => Type::U64,
            "f32" => Type::F32,
            "f64" => Type::F64,
            "bool" => Type::Bool,
            "string" => Type::String,
            "void" => Type::Void,
            name => {
                return self
                    .new_types
                    .get(name)
                    .map(|struct_type| Type::Struct(struct_type.clone()));
            }
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

        let mut arg_values = args
            .iter()
            .zip(func_type.params.iter())
            .map(|(expr, param_type)| {
                let value = self.build_expression(expr, Some(param_type.clone()))?;
                if value.1 != *param_type {
                    return CodeError::FunctionCallArgumentType(
                        name.0.clone(),
                        param_type.clone(),
                        value.1,
                    )
                    .err(expr.1);
                }

                Ok(value.0)
            })
            .collect::<Result<Vec<Value>, ManyErrors>>()?;

        if let Type::Struct(struct_type) = &*func_type.return_type {
            if func_type.is_extern {
                todo!("Extern return structs.");
            }
            let stack_slot_data = StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                struct_type.size(),
                struct_type.align_shift(),
            );
            let stack_slot = self.builder.create_sized_stack_slot(stack_slot_data);

            let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
            arg_values.push(struct_addr);
        }

        let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
        let inst = self.builder.ins().call(func_ref, &arg_values);
        match self.builder.inst_results(inst) {
            // Struct return
            [] if *func_type.return_type != Type::Void => {
                let return_dest = *arg_values
                    .last()
                    .expect("Return destination must be defined.");

                Ok((return_dest, *func_type.return_type))
            }
            [] => Ok((Value::from_u32(u32::MAX - 1), Type::Void)),
            [value] => Ok((*value, *func_type.return_type)),
            _ => panic!("Invalid number of return values. Must be 0 or 1."),
        }
    }

    fn build_struct_decl(
        &mut self,
        decl: &StructDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        let mut errors = Vec::new();
        if self.current_scope.parent.is_some() {
            errors.push(CodeError::StructDeclNotTopLevel.spanned(*span));
        }

        if self.type_from_name(&decl.name.0).is_some() {
            errors.push(CodeError::TypeRedeclaration(decl.name.0.clone()).spanned(decl.name.1));
        }

        let mut fields = IndexMap::new();
        let mut current_offset = 0;

        for (name, type_name) in &decl.fields {
            if fields.contains_key(&name.0) {
                errors.push(CodeError::StructDuplicateField(name.0.clone()).spanned(name.1));
                continue;
            }

            let Some(field_type) = self.type_from_name(&type_name.0) else {
                errors.push(CodeError::UnknownType(type_name.0.clone()).spanned(type_name.1));
                continue;
            };

            let field_size = field_type.size() as i32;
            let field_align = 1i32 << field_type.align_shift();
            let padding = (field_align - (current_offset % field_align)) % field_align;
            current_offset += padding;
            fields.insert(
                name.0.clone(),
                StructField {
                    r#type: field_type,
                    offset: current_offset,
                },
            );
            current_offset += field_size;
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let struct_type = StructType {
            name: decl.name.0.clone(),
            fields,
        };

        if self.verbose {
            println!("Defined struct: \n {struct_type:?}");
        }
        self.new_types.insert(decl.name.0.clone(), struct_type);
        Ok(())
    }

    fn get_extern_abi_param(&self, r#type: &Type) -> Option<AbiParam> {
        let mut purpose = ArgumentPurpose::Normal;
        let crane_type = match r#type {
            Type::I8 | Type::U8 => types::I8,
            Type::I16 | Type::U16 => types::I16,
            Type::I32 | Type::U32 => types::I32,
            Type::I64 | Type::U64 => types::I64,
            Type::F32 => types::F32,
            Type::F64 => types::F64,
            Type::String => self.isa.pointer_type(),
            Type::Bool => types::I8,
            Type::Struct(struct_type) => {
                // StructArgument must have a size that's a multiple of 8 byte
                purpose = ArgumentPurpose::StructArgument(struct_type.size_pad_8());
                match struct_type.size() {
                    0..=8 => types::I64,
                    9..=16 if self.isa.default_call_conv() == CallConv::SystemV => types::I128, // TODO: test this
                    _ => self.isa.pointer_type(),
                }
            }
            _ => return None,
        };

        Some(AbiParam::special(crane_type, purpose))
    }

    fn get_abi_param(&self, r#type: &Type) -> Option<AbiParam> {
        self.get_crane_type(r#type).map(AbiParam::new)
    }

    fn get_crane_type(&self, r#type: &Type) -> Option<types::Type> {
        Some(match r#type {
            Type::I8 | Type::U8 => types::I8,
            Type::I16 | Type::U16 => types::I16,
            Type::I32 | Type::U32 => types::I32,
            Type::I64 | Type::U64 => types::I64,
            Type::F32 => types::F32,
            Type::F64 => types::F64,
            Type::String => self.isa.pointer_type(),
            Type::Bool => types::I8,
            Type::Struct(_) => self.isa.pointer_type(),
            _ => return None,
        })
    }
}

fn combine_spans(a: SimpleSpan, b: SimpleSpan) -> SimpleSpan {
    SimpleSpan::new(a.context, a.start..b.end)
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}
