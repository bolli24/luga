use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    fs,
    hash::Hash,
    path::{Path, PathBuf},
};

use cranelift::{
    codegen::{Context, ir::ArgumentPurpose},
    module::{DataDescription, FuncId, Linkage, Module, default_libcall_names},
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{isa::CallConv, *},
};
use indexmap::{IndexMap, IndexSet};

use luga_syntax::{SimpleSpan, Span};
use luga_syntax::{
    parser::{
        BinOp, Conditional, EnumDecl, Expression, FunctionDecl, If, Intrinsic, Param, Program,
        Spanned, Statement, StructDecl, TopLevelItem, UnOp,
    },
    parsing::parse_source,
};

use crate::errors::{
    CodeError, CompileError, GeneratorErrors, MainModuleError, ManyErrors, ModuleErrors,
};
use crate::util::ResultCollectAll;

const UNREACHABLE: TrapCode = TrapCode::unwrap_user(1);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TypeId {
    local: usize,
    module: ModuleId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName(ModuleId, String);

impl Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
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
    Struct(TypeId),
    Enum(TypeId),
    Error,
}

impl Type {
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

fn can_coerce_type(actual: &Type, expected: &Type) -> bool {
    matches!(actual, Type::Never | Type::Error) || actual == expected
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
    name: QualifiedName,
    fields: IndexMap<String, StructField>,
    size: u32,
    align_shift: u8,
}

impl StructType {
    fn new(
        name: QualifiedName,
        fields: IndexMap<String, StructField>,
        type_context: &TypeContext,
    ) -> Self {
        let align_shift = fields
            .values()
            .map(|f| type_context.align_shift(&f.r#type))
            .max()
            .unwrap_or(0);

        let size = Self::compute_size(&fields, align_shift, type_context);

        Self {
            name,
            fields,
            size,
            align_shift,
        }
    }

    fn compute_size(
        fields: &IndexMap<String, StructField>,
        align_shift: u8,
        type_context: &TypeContext,
    ) -> u32 {
        let mut offset = 0;
        for field in fields.values() {
            let align = 1 << type_context.align_shift(&field.r#type);
            let padding = (align - (offset % align)) % align;
            offset += padding;
            offset += type_context.size(&field.r#type)
        }

        // Final padding so total size is a multiple of the struct's alignment
        let struct_align = 1 << align_shift;
        let padding = (struct_align - (offset % struct_align)) % struct_align;
        offset + padding
    }

    fn size_pad_8(&self) -> u32 {
        let size = self.size;
        if size.is_multiple_of(8) {
            size
        } else {
            size + (8 - size % 8)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    r#type: Type,
    offset: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    pub name: QualifiedName,
    pub variants: IndexSet<String>,
}

impl Type {
    fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayType<'a> {
        DisplayType { ty: self, types }
    }
}

struct DisplayType<'a> {
    ty: &'a Type,
    types: &'a TypeContext,
}

impl std::fmt::Display for DisplayType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ty {
            Type::Struct(id) | Type::Enum(id) => {
                let name = self.types.type_name(*id);
                write!(f, "{name}")
            }

            Type::Fn(func_type) => {
                write!(f, "{}", func_type.display(self.types))
            }
            ty => write!(f, "{ty}"),
        }
    }
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
            Self::Struct(id) => write!(f, "<struct {id:?}>"),
            Self::Enum(id) => write!(f, "<enum {id:?}>"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

impl FuncType {
    fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayFuncType<'a> {
        DisplayFuncType { func: self, types }
    }
}

struct DisplayFuncType<'a> {
    func: &'a FuncType,
    types: &'a TypeContext,
}

impl Display for DisplayFuncType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}fn {} ({}) : {}",
            if self.func.is_extern { "extern " } else { "" },
            self.func.name,
            self.func
                .params
                .iter()
                .map(|param| param.display(self.types).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            self.func.return_type.display(self.types)
        )
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

impl StructType {
    fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayStructType<'a> {
        DisplayStructType {
            struct_type: self,
            types,
        }
    }
}

struct DisplayStructType<'a> {
    struct_type: &'a StructType,
    types: &'a TypeContext,
}

impl Display for DisplayStructType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "struct {} {{ {} }}",
            self.struct_type.name,
            self.struct_type
                .fields
                .iter()
                .map(|field| {
                    format!(
                        "{}@{}: {}",
                        field.0,
                        field.1.offset,
                        field.1.r#type.display(self.types)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
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

impl EnumType {
    fn display<'a>(&'a self, _types: &'a TypeContext) -> DisplayEnumType<'a> {
        DisplayEnumType { enum_type: self }
    }
}

struct DisplayEnumType<'a> {
    enum_type: &'a EnumType,
}

impl Display for DisplayEnumType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "enum {} {{ {} }}",
            self.enum_type.name,
            self.enum_type
                .variants
                .iter()
                .map(|variant| variant.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

impl Display for EnumType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "enum {} {{ {} }}",
            self.name,
            self.variants
                .iter()
                .map(|variant| variant.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

struct TypeContext {
    pub structs: HashMap<TypeId, StructType>,
    pub enums: HashMap<TypeId, EnumType>,
    pub associated_functions: HashMap<TypeId, HashMap<String, Symbol>>,
    next_type_id: usize,
}

impl TypeContext {
    fn new() -> Self {
        Self {
            structs: HashMap::new(),
            enums: HashMap::new(),
            associated_functions: HashMap::new(),
            next_type_id: 0,
        }
    }

    fn alloc_type_id(&mut self, module: ModuleId) -> TypeId {
        let id = TypeId {
            local: self.next_type_id,
            module,
        };
        self.next_type_id += 1;
        id
    }

    fn type_name(&self, id: TypeId) -> String {
        if let Some(struct_type) = self.structs.get(&id) {
            return struct_type.name.to_string();
        }
        if let Some(enum_type) = self.enums.get(&id) {
            return enum_type.name.to_string();
        }
        format!("<type {id:?}>")
    }

    fn layout(&self, r#type: &Type) -> (u32, u8) {
        match r#type {
            Type::I8 | Type::U8 => (1, 0),
            Type::I16 | Type::U16 => (2, 1),
            Type::I32 | Type::U32 => (4, 2),
            Type::I64 | Type::U64 => (8, 3),
            Type::F32 => (4, 2),
            Type::F64 => (8, 3),
            Type::Void | Type::Never => (0, 0),
            Type::String => (8, 3), // TODO: platform pointer size
            Type::Bool => (1, 0),
            Type::Fn(_) => (0, 0),
            Type::Struct(struct_id) => {
                let struct_type = &self.structs[struct_id];
                (struct_type.size, struct_type.align_shift)
            }
            Type::Enum(_) => self.layout(&Type::U32),
            Type::Error => (0, 0),
        }
    }

    fn size(&self, r#type: &Type) -> u32 {
        self.layout(r#type).0
    }

    fn align_shift(&self, r#type: &Type) -> u8 {
        self.layout(r#type).1
    }

    fn struct_type(&self, r#type: &Type) -> Option<&StructType> {
        match r#type {
            Type::Struct(struct_id) => self.structs.get(struct_id),
            _ => None,
        }
    }
}

pub fn generate(
    program: Program,
    file_path: Option<&Path>,
    src: &str,
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
    let mut data_desc = DataDescription::new();
    let string_data = b"%d\n\0";
    data_desc.define(string_data.to_vec().into_boxed_slice());
    let string_data_id = module
        .declare_data("format_string", Linkage::Local, true, false)
        .expect("Failed to declare string data");

    module
        .define_data(string_data_id, &data_desc)
        .expect("Failed to define string data");

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

    let state = GeneratorState {
        isa,
        module: &mut module,
        current_module: ModuleId(0),
        strings: 0,
        printf_id,
        exit_id,
        verbose,
        current_scope: Scope::new(),
        dir: file_path
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        fn_name_prefix: String::new(),
        compiling_imports: HashSet::new(),
        compiled_imports: HashMap::new(),
        types: TypeContext::new(),
    };

    let mut mod_errors = Vec::new();

    match state.build_main_module(program, &mut mod_errors) {
        Err(MainModuleError::MissingMain) => return Err(GeneratorErrors::MissingMain),
        Err(MainModuleError::CodeErrors(errors)) => {
            mod_errors.push(ModuleErrors {
                path: file_path.map(Path::to_path_buf),
                src: src.to_owned(),
                errors: errors
                    .into_iter()
                    .map(|err| CompileError::Code(err))
                    .collect(),
            });
            return Err(GeneratorErrors::ModuleErrors(mod_errors));
        }
        Ok(_) => {}
    }

    Ok(module.finish())
}

struct Scope {
    symbols: HashMap<String, Symbol>,
    parent: Option<Box<Scope>>,
    is_function_boundary: bool,
}

#[derive(Clone)]
pub enum Symbol {
    Variable(Variable, Type),
    Function(FuncId, FuncType),
    Type(Type),
    Module(HashMap<String, Symbol>), // TODO: store module symbol table outside and use a ModuleId to refer to it here
}

pub enum LookupResult<'a> {
    Found(&'a Symbol),
    CaptureError,
    NotFound,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            symbols: HashMap::new(),
            parent: None,
            is_function_boundary: false,
        }
    }

    pub fn function_scope(self) -> Self {
        Self {
            symbols: HashMap::new(),
            parent: Some(Box::new(self)),
            is_function_boundary: true,
        }
    }

    pub fn get_symbol(&self, name: &str) -> LookupResult<'_> {
        self.get_symbol_inner(name, false)
    }

    fn get_symbol_inner(&self, name: &str, crossed_boundary: bool) -> LookupResult<'_> {
        match self.symbols.get(name) {
            Some(Symbol::Variable(..)) if crossed_boundary => return LookupResult::CaptureError,
            Some(s) => return LookupResult::Found(s),
            None => {}
        }
        match self.parent.as_ref() {
            None => LookupResult::NotFound,
            Some(parent) => {
                parent.get_symbol_inner(name, crossed_boundary || self.is_function_boundary)
            }
        }
    }

    pub fn insert_symbol(&mut self, name: String, symbol: Symbol) -> Result<(), ()> {
        use std::collections::hash_map::Entry;

        match self.symbols.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(symbol);
                Ok(())
            }
            Entry::Occupied(_) => Err(()),
        }
    }

    pub fn get_function(&self, name: &str) -> Option<(FuncId, &FuncType)> {
        match self.get_symbol(name) {
            LookupResult::Found(Symbol::Function(id, ft)) => Some((*id, ft)),
            _ => None,
        }
    }

    pub fn get_type(&self, name: &str) -> Option<&Type> {
        match self.get_symbol(name) {
            LookupResult::Found(Symbol::Type(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_module(&self, name: &str) -> Option<&HashMap<String, Symbol>> {
        match self.get_symbol(name) {
            LookupResult::Found(Symbol::Module(m)) => Some(m),
            _ => None,
        }
    }

    pub fn get_module_symbol(&self, module_name: &str, symbol_name: &str) -> Option<&Symbol> {
        self.get_module(module_name)?.get(symbol_name)
    }
}

struct GeneratorState<'a> {
    isa: std::sync::Arc<dyn isa::TargetIsa>,
    module: &'a mut ObjectModule,
    current_module: ModuleId,
    strings: usize,
    printf_id: FuncId,
    exit_id: FuncId,
    verbose: bool,
    current_scope: Scope,
    dir: PathBuf,
    fn_name_prefix: String,
    compiling_imports: HashSet<PathBuf>,
    compiled_imports: HashMap<PathBuf, HashMap<String, Symbol>>,
    types: TypeContext,
}

struct FnGenerator<'a, 'ctx> {
    state: &'ctx mut GeneratorState<'a>,
    builder: FunctionBuilder<'ctx>,
    var_count: usize,
    current_func_type: FuncType,
    return_dest: Option<Variable>,
}

impl GeneratorState<'_> {
    fn build_main_module(
        mut self,
        program: Program,
        mod_errors: &mut Vec<ModuleErrors>,
    ) -> Result<HashMap<String, Symbol>, MainModuleError> {
        let found_main = self
            .pass1(&program, mod_errors, true)
            .map_err(|errors| MainModuleError::CodeErrors(errors))?;

        if !found_main {
            return Err(MainModuleError::MissingMain);
        }

        let mut errors = ManyErrors::new();

        self.pass2(&program, &mut errors);

        if !errors.is_empty() {
            return Err(MainModuleError::CodeErrors(errors));
        }

        Ok(self.current_scope.symbols)
    }

    fn pass1(
        &mut self,
        program: &Program,
        mod_errors: &mut Vec<ModuleErrors>,
        is_entry: bool,
    ) -> Result<bool, ManyErrors> {
        let mut errors = ManyErrors::new();

        let mut found_main = false;
        for item in &program.items {
            match &item.0 {
                TopLevelItem::FunctionDecl(decl) => {
                    if is_entry && decl.name == "main" && !decl.r#extern {
                        found_main = true;
                    }

                    if let Err(mut e) = self.pre_declare_fn(decl, &item.1, is_entry) {
                        errors.append(&mut e);
                    }
                }
                TopLevelItem::StructDecl(decl) => {
                    if let Err(mut e) = self.build_struct_decl(decl) {
                        errors.append(&mut e);
                    }
                }
                TopLevelItem::EnumDecl(decl) => {
                    if let Err(mut e) = self.build_enum_decl(decl) {
                        errors.append(&mut e);
                    }
                }
                TopLevelItem::Import(path, rename) => {
                    if let Err(CompileError::Code(e)) =
                        self.process_import(path, rename.as_ref(), item.1, mod_errors)
                    {
                        errors.push(e);
                    }
                }
            }
        }

        if !errors.is_empty() {
            Err(errors)
        } else {
            Ok(found_main)
        }
    }

    fn pass2(&mut self, program: &Program, errors: &mut ManyErrors) {
        for item in &program.items {
            if let TopLevelItem::FunctionDecl(decl) = &item.0 {
                if let Err(mut e) = self.build_fn_body(decl, &item.1, true) {
                    errors.append(&mut e);
                }
            }
        }
    }

    fn process_import(
        &mut self,
        path: &[Spanned<String>],
        rename: Option<&Spanned<String>>,
        import_span: SimpleSpan,
        mod_errors: &mut Vec<ModuleErrors>,
    ) -> Result<(), CompileError> {
        let module_name = rename
            .map(|r| r.0.clone())
            .unwrap_or_else(|| path.last().unwrap().0.clone());
        let module_name_span = rename
            .map(|r| r.1)
            .unwrap_or_else(|| path.last().unwrap().1);
        let joined_path: PathBuf = path
            .iter()
            .fold(self.dir.clone(), |acc, seg| acc.join(&seg.0))
            .with_extension("luga");

        let path_str = joined_path.display().to_string();
        let canonical = std::fs::canonicalize(&joined_path).map_err(|_| {
            CompileError::Code(CodeError::ImportNotFound(path_str.clone()).spanned(import_span))
        })?;

        if self.compiling_imports.contains(&canonical) {
            return Err(CodeError::CircularImport(path_str)
                .spanned(import_span)
                .into());
        }
        // Now we start compiling the module itself.
        let symbols = if let Some(cached) = self.compiled_imports.get(&canonical) {
            cached.clone()
        } else {
            let src = fs::read_to_string(&canonical).map_err(|err| {
                CodeError::ImportError(path_str.clone(), err.to_string()).spanned(import_span)
            })?;

            let program = match parse_source(&src, self.verbose) {
                Ok(program) => program,
                Err(failure) => {
                    mod_errors.push(ModuleErrors {
                        path: Some(canonical),
                        src: src.to_owned(),
                        errors: CompileError::from_parse_failure(failure),
                    });
                    return Err(CompileError::ModuleInner);
                }
            };
            let dir = joined_path.parent().unwrap_or(Path::new(".")).to_path_buf();

            self.compiling_imports.insert(canonical.clone());
            let symbols = match self.compile_import(program, &dir, &canonical, mod_errors) {
                Ok(compiled) => compiled,
                Err(errors) => {
                    mod_errors.push(ModuleErrors {
                        path: Some(canonical),
                        src: src.to_owned(),
                        errors: errors.into_iter().map(CompileError::Code).collect(),
                    });
                    return Err(CompileError::ModuleInner);
                }
            };
            self.compiling_imports.remove(&canonical);
            self.compiled_imports
                .insert(canonical.clone(), symbols.clone());
            symbols
        };

        self.current_scope
            .insert_symbol(module_name.clone(), Symbol::Module(symbols))
            .map_err(|_| {
                CodeError::NameAlreadyDefined(module_name)
                    .spanned(module_name_span)
                    .into()
            })
    }

    fn compile_import(
        &mut self,
        program: Program,
        dir: &Path,
        canonical: &Path,
        mod_errors: &mut Vec<ModuleErrors>,
    ) -> Result<HashMap<String, Symbol>, ManyErrors> {
        let new_module = ModuleId(self.current_module.0 + 1);
        let saved_module = std::mem::replace(&mut self.current_module, new_module);
        let saved_scope = std::mem::replace(&mut self.current_scope, Scope::new());
        let saved_dir = std::mem::replace(&mut self.dir, dir.to_path_buf());
        let new_prefix = module_prefix(canonical, self.compiled_imports.len());
        let saved_prefix = std::mem::replace(&mut self.fn_name_prefix, new_prefix);

        let mut errors = ManyErrors::new();

        if let Err(errors) = self.pass1(&program, mod_errors, false) {
            self.current_module = saved_module;
            self.current_scope = saved_scope;
            self.dir = saved_dir;
            self.fn_name_prefix = saved_prefix;
            return Err(errors);
        }

        self.pass2(&program, &mut errors);

        let module_symbols = std::mem::replace(&mut self.current_scope, saved_scope).symbols;
        self.current_module = saved_module;
        self.dir = saved_dir;
        self.fn_name_prefix = saved_prefix;

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(module_symbols)
    }
}

fn module_prefix(canonical_path: &Path, module_index: usize) -> String {
    let stem = canonical_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}{module_index}$")
}

impl<'a, 'ctx> FnGenerator<'a, 'ctx> {
    fn build_statements(&mut self, statements: &[Spanned<Statement>]) -> Result<bool, ManyErrors> {
        let mut errors = ManyErrors::new();
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
                if self.state.current_scope.symbols.contains_key(name) {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                };

                let value = match self.build_expression(expr, None) {
                    Ok(value) => value,
                    Err(errors) => {
                        self.new_error_var(name.to_owned());
                        return Err(errors);
                    }
                };

                if value.1 == Type::Error {
                    self.new_error_var(name.to_owned());
                    return Ok(false);
                }

                let Some(var_type) = self.state.get_crane_type(&value.1) else {
                    return CodeError::BindingType(value.1.to_string()).err(expr.1);
                };

                let var = self.builder.declare_var(var_type);
                self.builder.def_var(var, value.0);
                self.new_var(name.to_owned(), var, value);
            }
            Statement::Assignment(name, expr) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(Symbol::Variable(var, var_type)) => {
                    let (var, var_type) = (*var, var_type.clone());
                    let value = self.build_expression(expr, Some(var_type.clone()))?;

                    if !can_coerce_type(&value.1, &var_type) {
                        return CodeError::AssignmentIncompatibleTypes(
                            var_type.to_string(),
                            value.1.to_string(),
                        )
                        .err(statement.1);
                    }

                    if matches!(value.1, Type::Never | Type::Error) {
                        return Ok(value.1 == Type::Never);
                    }

                    self.builder.def_var(var, value.0);
                }
                LookupResult::CaptureError => {
                    return CodeError::Capture(name.clone()).err(statement.1);
                }
                _ => {
                    return CodeError::UnknownVariable(name.clone()).err(statement.1);
                }
            },
            Statement::Block(statements) => {
                let new_block = self.builder.create_block();
                self.builder.ins().jump(new_block, []);
                self.builder.switch_to_block(new_block);
                self.builder.seal_block(new_block);
                self.state.push_scope();

                let result = self.build_statements(statements);

                if self.state.pop_scope().is_none() {
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
                return self.state.build_fn_decl(decl, &statement.1).map(|_| false);
            }
            Statement::StructDecl(decl) => {
                return self.state.build_struct_decl(decl).map(|_| false);
            }
            Statement::EnumDecl(decl) => {
                return self.state.build_enum_decl(decl).map(|_| false);
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
        let field_align_shift = self.state.types.align_shift(&struct_field.r#type);
        *current_offset = Self::align_to(*current_offset, field_align_shift);

        match &struct_field.r#type {
            Type::Struct(struct_id) => {
                let struct_type = self.state.types.structs[struct_id].clone();
                // For nested structs, recursively copy each field
                for field in struct_type.fields.values() {
                    self.build_return_field_copy(field, current_offset, struct_addr, return_dest);
                }
            }
            _ => {
                // Copy primitive field
                let crane_type = self
                    .state
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
                *current_offset += self.state.types.size(&struct_field.r#type) as i32;
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
        if value.1 == Type::Error {
            return Ok(false);
        }
        if !can_coerce_type(&value.1, &self.current_func_type.return_type) {
            return CodeError::InvalidReturnType(
                self.current_func_type.return_type.to_string(),
                value.1.to_string(),
            )
            .err(expr.1);
        }
        match value.1 {
            // Diverging expression already terminated the block — nothing to emit.
            Type::Never => {}
            Type::Void => {
                self.builder.ins().return_(&[]);
            }
            Type::Struct(struct_id) => {
                let struct_type = &self.state.types.structs[&struct_id].clone();
                let return_dest = self
                    .return_dest
                    .expect("Return destination must be valid when returning struct.");

                let return_dest = self.builder.use_var(return_dest);

                let mut current_offset = 0;

                for field in struct_type.fields.values() {
                    self.build_return_field_copy(field, &mut current_offset, value.0, return_dest);
                }

                self.builder.ins().return_(&[]);
            }
            _ => {
                self.builder.ins().return_(&[value.0]);
            }
        };
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
                        CodeError::CoerseInt(out_type.to_string(), *value).err(*span)
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
                            CodeError::CoerseInt(Type::U64.to_string(), *value).err(*span)
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

                let string_id = self.state.strings;
                self.state.strings += 1;
                let string_data_id = self
                    .state
                    .module
                    .declare_data(&format!("string_{string_id}"), Linkage::Local, true, false)
                    .expect("Failed to declare string data");

                self.state
                    .module
                    .define_data(string_data_id, &data_desc)
                    .expect("Failed to define string data");

                let format_string_ref = self
                    .state
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

                if x_value.1 == Type::Never || y_value.1 == Type::Never {
                    return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
                }

                if x_value.1 == Type::Error || y_value.1 == Type::Error {
                    let value = self.builder.ins().iconst(types::I32, 0);
                    return Ok((value, Type::Error));
                }

                if x_value.1 != y_value.1 {
                    return CodeError::IncompatipleBinOpTypes(
                        *op,
                        x_value.1.to_string(),
                        y_value.1.to_string(),
                    )
                    .err(*span);
                }

                let result = match x_value.1 {
                    Type::I8 | Type::I32 | Type::I64 => {
                        self.build_int_bin_op(*op, x_value.0, y_value.0, x_value.1)
                    }
                    Type::F32 | Type::F64 => {
                        self.build_float_bin_op(*op, x_value.0, y_value.0, x_value.1)
                    }
                    Type::Bool => self.build_bool_bin_op(*op, x_value.0, y_value.0),
                    Type::Enum(enum_id) if matches!(*op, BinOp::Equal | BinOp::NotEqual) => {
                        let enum_type = &self.state.types.enums[&enum_id];
                        let result = if enum_type.variants.len() <= 1 {
                            let always = if *op == BinOp::Equal { 1 } else { 0 };
                            self.builder.ins().iconst(types::I8, always)
                        } else {
                            let cc = if *op == BinOp::Equal {
                                IntCC::Equal
                            } else {
                                IntCC::NotEqual
                            };
                            self.builder.ins().icmp(cc, x_value.0, y_value.0)
                        };
                        Some((result, Type::Bool))
                    }
                    _ => None,
                }
                .ok_or_else(|| {
                    CodeError::InvalidBinOpType(*op, y_value.1.to_string()).spanned(*span)
                })?;
                Ok(result)
            }
            Expression::Unary(op, expr) => {
                let value = self.build_expression(expr.as_ref(), try_into_type)?;
                if matches!(value.1, Type::Never | Type::Error) {
                    return Ok(value);
                }

                if value.1 == Type::Bool && *op == UnOp::Not {
                    let result = self.builder.ins().bxor_imm(value.0, 1);
                    Ok((result, Type::Bool))
                } else {
                    CodeError::InvalidUnOpType(*op, value.1.to_string()).err(*span)
                }
            }
            Expression::Intrinsic(intrinsic, args) => match intrinsic {
                Intrinsic::Print => self.build_printf(span, args, false),
                Intrinsic::Println => self.build_printf(span, args, true),
                Intrinsic::Assert => self.build_assert(span, args),
                Intrinsic::Exit => self.build_exit(span, args),
                Intrinsic::EnumToInt => self.build_enum_to_int(span, args),
            },
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(Symbol::Variable(var, var_type)) => {
                    Ok((self.builder.use_var(*var), var_type.clone()))
                }
                LookupResult::Found(Symbol::Function(..) | Symbol::Type(_)) => {
                    CodeError::TypeUsedAsValue(name.clone()).err(*span)
                }
                LookupResult::Found(Symbol::Module(..)) => {
                    CodeError::ModuleUsedAsValue(name.clone()).err(*span)
                }
                LookupResult::CaptureError => CodeError::Capture(name.clone()).err(*span),
                LookupResult::NotFound => CodeError::UnknownVariable(name.clone()).err(*span),
            },
            Expression::FunctionCall(name, args) => self.build_fn_call(name, args, span),
            Expression::MemberAccess(expr, member_name) => {
                if let Some(symbol) = self.resolve_expr_as_symbol(expr).cloned() {
                    return match symbol {
                        Symbol::Module(_) => {
                            CodeError::ModuleUsedAsValue(member_name.0.clone()).err(member_name.1)
                        }
                        Symbol::Type(Type::Enum(enum_id)) => {
                            let Some(idx) = self.state.types.enums[&enum_id]
                                .variants
                                .get_index_of(&member_name.0)
                            else {
                                return CodeError::UnknownVariant(
                                    self.state.types.type_name(enum_id),
                                    member_name.0.clone(),
                                )
                                .err(member_name.1);
                            };
                            let val = self.builder.ins().iconst(types::I32, idx as i64);
                            return Ok((val, Type::Enum(enum_id)));
                        }
                        Symbol::Type(Type::Struct(struct_id)) => {
                            CodeError::TypeUsedAsValue(self.state.types.type_name(struct_id))
                                .err(expr.1)
                        }
                        Symbol::Type(_) => {
                            unreachable!("Only struct and enum types may be imported.")
                        }
                        Symbol::Variable(..) => {
                            unreachable!("resolve_expr_as_symbol must not return variables.")
                        }
                        Symbol::Function(_, func_type) => {
                            CodeError::FunctionUsedAsValue(func_type.name.clone()).err(expr.1)
                        }
                    };
                }

                let expr_value = self.build_expression(expr, None)?;
                if matches!(expr_value.1, Type::Never | Type::Error) {
                    return Ok(expr_value);
                }
                let name = member_name;
                let field = match &expr_value.1 {
                    Type::Struct(struct_id) => {
                        self.state.types.structs[struct_id].fields.get(&name.0)
                    }
                    _ => None,
                }
                .ok_or_else(|| {
                    CodeError::UnknownField(expr_value.1.to_string(), name.0.clone())
                        .spanned(name.1)
                })?;

                let field_crane_type = self
                    .state
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
                let mut errors = ManyErrors::new();
                let mut arg_values = HashMap::with_capacity(struct_init.args.len());

                let path = &struct_init.path;
                let path_span = combine_spans(path.first().unwrap().1, path.last().unwrap().1);
                let r#type = self.state.type_from_path(path);

                for arg in &struct_init.args {
                    let maybe_type = r#type
                        .as_ref()
                        .and_then(|r#type| self.state.types.struct_type(r#type))
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

                let Some(r#type) = self.state.type_from_path(path) else {
                    let type_name = path
                        .iter()
                        .map(|s| s.0.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    errors.push(CodeError::UnknownType(type_name).spanned(path_span));
                    return Err(errors);
                };

                let Type::Struct(struct_id) = &r#type else {
                    errors.push(CodeError::NotStructType(r#type.to_string()).spanned(path_span));
                    return Err(errors);
                };

                if !errors.is_empty() {
                    return Err(errors);
                }

                let struct_type = &self.state.types.structs[struct_id];

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    struct_type.size,
                    struct_type.align_shift,
                ));

                let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
                let mut poison_type = None;

                for (name, field) in &struct_type.fields {
                    let Some(value) = arg_values.get(name) else {
                        errors.push(CodeError::StructInitMissingArg(name.clone()).spanned(*span));
                        continue;
                    };

                    if !can_coerce_type(&value.1, &field.r#type) {
                        errors.push(
                            CodeError::StructInitInvalidType(
                                field.r#type.to_string(),
                                value.1.to_string(),
                            )
                            .spanned(*span),
                        );
                        continue;
                    }

                    if matches!(value.1, Type::Never | Type::Error) {
                        poison_type = Some(value.1.clone());
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

                if let Some(poison_type) = poison_type {
                    return Ok((Value::from_u32(u32::MAX - 1), poison_type));
                }

                Ok((struct_addr, r#type))
            }
        }
    }

    fn resolve_expr_as_symbol(&self, expr: &Spanned<Expression>) -> Option<&Symbol> {
        match &expr.0 {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(
                    sym @ (Symbol::Module(_) | Symbol::Type(_) | Symbol::Function(..)),
                ) => Some(sym),
                _ => None,
            },
            Expression::MemberAccess(base, member) => match self.resolve_expr_as_symbol(base)? {
                Symbol::Module(map) => map.get(&member.0),
                Symbol::Type(Type::Struct(type_id) | Type::Enum(type_id)) => self
                    .state
                    .types
                    .associated_functions
                    .get(type_id)?
                    .get(&member.0),
                _ => None,
            },
            _ => None,
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
                BinOp::NotEqual => self.builder.ins().icmp(IntCC::NotEqual, x, y),
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
                BinOp::Equal
                | BinOp::NotEqual
                | BinOp::Lt
                | BinOp::Gt
                | BinOp::Lte
                | BinOp::Gte => Type::Bool,
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
                BinOp::NotEqual => self.builder.ins().fcmp(FloatCC::NotEqual, x, y),
                BinOp::Lt => self.builder.ins().fcmp(FloatCC::LessThan, x, y),
                BinOp::Gt => self.builder.ins().fcmp(FloatCC::GreaterThan, x, y),
                BinOp::Lte => self.builder.ins().fcmp(FloatCC::LessThanOrEqual, x, y),
                BinOp::Gte => self.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, x, y),
            },
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => r#type,
                BinOp::Equal
                | BinOp::NotEqual
                | BinOp::Lt
                | BinOp::Gt
                | BinOp::Lte
                | BinOp::Gte => Type::Bool,
            },
        ))
    }

    #[must_use]
    fn build_bool_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> Option<(Value, Type)> {
        Some(match op {
            BinOp::Equal => (self.builder.ins().icmp(IntCC::Equal, x, y), Type::Bool),
            BinOp::NotEqual => (self.builder.ins().icmp(IntCC::NotEqual, x, y), Type::Bool),
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
                    const PRINT_ARG_TYPE_NAMES: [&str; 10] = [
                        "u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64", "bool", "string",
                    ];
                    if matches!(value.1, Type::Never | Type::Error) {
                        return Ok((value.0, value.1.clone()));
                    }
                    if !PRINT_ARG_TYPES.contains(&value.1) && !matches!(value.1, Type::Enum(_)) {
                        return CodeError::IntrisicArgumentType(
                            "print",
                            &PRINT_ARG_TYPE_NAMES,
                            value.1.to_string(),
                        )
                        .err(expr.1);
                    }
                }
                result
            })
            .collect_all()
            .map_err(|errors| errors.into_iter().flatten().collect::<ManyErrors>())?;

        if arg_values.iter().any(|(_, ty)| *ty == Type::Never) {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
        }

        if arg_values.iter().any(|(_, ty)| *ty == Type::Error) {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
        }

        let format_string_value = self.build_expression(&format_string_arg, None)?;
        debug_assert_eq!(format_string_value.1, Type::String);

        // Because printf is a variadic function we have to change it's signature on the fly
        // See: adjust_call_for_c_variadic() in rustc_codegen_cranelift
        let mut params = Vec::with_capacity(arg_values.len() + 1);
        params.push(AbiParam::new(self.state.isa.pointer_type()));
        params.extend(arg_values.iter().map(|(_, t)| {
            self.state
                .get_extern_abi_param(t)
                .expect("Type must be valid @print parameter.")
        }));

        let printf_ref = self
            .state
            .module
            .declare_func_in_func(self.state.printf_id, self.builder.func);

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

    fn new_var(&mut self, name: String, var: Variable, value: (Value, Type)) {
        self.state
            .current_scope
            .insert_symbol(name, Symbol::Variable(var, value.1))
            .expect("Variable must not exist.");
        self.var_count += 1;
    }

    fn new_error_var(&mut self, name: String) {
        let var = self.builder.declare_var(types::I32);
        let value = self.builder.ins().iconst(types::I32, 0);
        self.builder.def_var(var, value);
        self.new_var(name, var, (value, Type::Error));
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

        let mut errors = ManyErrors::new();

        self.builder.ins().jump(condition_blocks[0], []);

        let mut returned = else_block.is_some();

        for (index, conditional) in if_statement.if_branches.iter().enumerate() {
            let condition_block = condition_blocks[index];
            let next_block: Block = condition_blocks
                .get(index + 1)
                .copied()
                .or(else_block)
                .unwrap_or(exit_block);

            self.builder.seal_block(condition_block);
            self.builder.switch_to_block(condition_block);
            let condition_value = match self.build_expression(&conditional.condition, None) {
                Ok((_, Type::Never)) => {
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Ok((_, Type::Error)) => {
                    returned = false;
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Ok(condition_value) if can_coerce_type(&condition_value.1, &Type::Bool) => {
                    condition_value
                }
                Ok((_, r#type)) => {
                    errors.push(
                        CodeError::InvalidConditionType(r#type.to_string())
                            .spanned(conditional.condition.1),
                    );
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
            };

            let then_block = self.builder.create_block();
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

    fn build_while(&mut self, conditional: &Conditional) -> Result<bool, ManyErrors> {
        let Conditional { condition, block } = conditional;

        let while_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        let condition_block = self.builder.create_block();
        self.builder.ins().jump(condition_block, []);
        self.builder.switch_to_block(condition_block);

        let condition_value = self.build_expression(condition, None)?;
        if matches!(condition_value.1, Type::Never | Type::Error) {
            self.builder.ins().jump(exit_block, []);
            self.builder.switch_to_block(exit_block);
            self.builder.seal_block(condition_block);
            self.builder.seal_block(while_block);
            self.builder.seal_block(exit_block);
            return Ok(false);
        }
        if !can_coerce_type(&condition_value.1, &Type::Bool) {
            return CodeError::InvalidConditionType(condition_value.1.to_string()).err(condition.1);
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
        self.state.push_scope();

        let result = self.build_statements(body);

        let Some(_) = self.state.pop_scope() else {
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
        if condition.1 == Type::Never {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
        }
        if condition.1 == Type::Error {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
        }
        if !can_coerce_type(&condition.1, &Type::Bool) {
            return CodeError::IntrisicArgumentType("assert", &["bool"], condition.1.to_string())
                .err(args[0].1);
        }

        let exit_code = self.builder.ins().iconst(types::I32, 1);

        // TODO: use abort instead of exit
        let exit_ref = self
            .state
            .module
            .declare_func_in_func(self.state.exit_id, self.builder.func);

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
        if expr.1 == Type::Never {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
        }
        if expr.1 == Type::Error {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
        }
        if !can_coerce_type(&expr.1, &Type::I32) {
            return CodeError::IntrisicArgumentType("exit", &["i32"], expr.1.to_string())
                .err(args[0].1);
        }

        let exit_ref = self
            .state
            .module
            .declare_func_in_func(self.state.exit_id, self.builder.func);

        self.builder.ins().call(exit_ref, &[expr.0]);
        Ok((Value::from_u32(u32::MAX - 1), Type::Never))
    }

    fn build_enum_to_int(
        &mut self,
        span: &SimpleSpan,
        args: &[Spanned<Expression>],
    ) -> Result<(Value, Type), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("enumToInt", 1, args.len()).err(*span);
        }

        let expr = self.build_expression(&args[0], None)?;
        if expr.1 == Type::Never {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
        }
        if expr.1 == Type::Error {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
        }
        if !matches!(expr.1, Type::Enum(_)) {
            return CodeError::IntrinsicExpectedEnum("enumToInt", expr.1.to_string())
                .err(args[0].1);
        }

        Ok((expr.0, Type::I32))
    }
}

impl GeneratorState<'_> {
    fn push_scope(&mut self) {
        let old_scope = std::mem::replace(&mut self.current_scope, Scope::new());
        self.current_scope.parent = Some(Box::new(old_scope));
    }

    fn pop_scope(&mut self) -> Option<Scope> {
        if let Some(mut parent) = self.current_scope.parent.take() {
            std::mem::swap(&mut self.current_scope, &mut *parent);
            Some(*parent)
        } else {
            None
        }
    }

    // Declare a function's signature in the module
    // Does not build the body — call build_fn_body separately for that.
    fn make_function_symbol(
        &mut self,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        object_name: &str,
        linkage: Linkage,
        is_top_level: bool,
    ) -> Result<Symbol, ManyErrors> {
        let FunctionDecl {
            name,
            r#extern: is_extern,
            params,
            body,
            return_type,
        } = decl;

        if *is_extern {
            assert!(body.is_empty());
        }

        let is_main = is_top_level && !*is_extern && name == "main";

        let mut fn_params: Vec<(String, Type, ArgumentPurpose)> = Vec::new();
        let mut errors = ManyErrors::new();
        let mut param_types = Vec::new();

        if is_main && !params.is_empty() {
            errors.push(CodeError::MainHasParams.spanned(*span));
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
            let purpose = if let Type::Struct(struct_id) = &r#type
                && *is_extern
            {
                let struct_type = &self.types.structs[struct_id];
                ArgumentPurpose::StructArgument(struct_type.size_pad_8())
            } else {
                ArgumentPurpose::Normal
            };
            fn_params.push((name.0.clone(), r#type, purpose));
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let declared_return_type = match return_type {
            Some(type_name) => self
                .type_from_name(&type_name.0)
                .ok_or(CodeError::UnknownType(type_name.0.clone()).spanned(type_name.1))?,
            None => Type::Void,
        };

        // fn main must return i32 (or have no return annotation, implying i32 with return 0).
        let (return_type, _main_implicit_zero) = if is_main {
            match &declared_return_type {
                Type::Void => (Type::I32, true),
                Type::I32 => (Type::I32, false),
                other => {
                    errors.push(CodeError::MainInvalidReturnType(other.to_string()).spanned(*span));
                    (Type::I32, false)
                }
            }
        } else {
            (declared_return_type, false)
        };

        if !errors.is_empty() {
            return Err(errors);
        }

        if self.types.size(&return_type) > 8 {
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

        let func_id = self
            .module
            .declare_function(object_name, linkage, &signature)
            .expect("Function must be declarable.");

        /* self.current_scope
        .insert_symbol(
            name.to_owned(),
            Symbol::Function(func_id, func_type.clone()),
        )
        .expect("Must not overwrite symbol"); */

        Ok(Symbol::Function(func_id, func_type))
    }

    fn pre_declare_fn(
        &mut self,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        is_top_level: bool,
    ) -> Result<(), ManyErrors> {
        if let LookupResult::Found(_) = self.current_scope.get_symbol(&decl.name) {
            return Err(CodeError::NameAlreadyDefined(decl.name.to_owned())
                .spanned(*span)
                .into());
        }

        let object_name = if decl.r#extern {
            decl.name.clone()
        } else {
            format!("{}{}", self.fn_name_prefix, decl.name)
        };

        let linkage = if decl.r#extern {
            Linkage::Import
        } else if is_top_level && decl.name == "main" {
            Linkage::Export
        } else {
            Linkage::Local
        };

        let symbol = self.make_function_symbol(decl, span, &object_name, linkage, is_top_level)?;

        self.current_scope
            .insert_symbol(decl.name.clone(), symbol)
            .expect("Must not overwrite symbol");

        Ok(())
    }

    fn pre_declare_assoc_fn(
        &mut self,
        owner: TypeId,
        owner_name: &str,
        decl: &FunctionDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        if self
            .types
            .associated_functions
            .get(&owner)
            .is_some_and(|map| map.contains_key(&decl.name))
        {
            return Err(CodeError::NameAlreadyDefined(decl.name.clone())
                .spanned(*span)
                .into());
        }

        let object_name = format!("{}{}${}", self.fn_name_prefix, owner_name, decl.name);

        let symbol = self.make_function_symbol(decl, span, &object_name, Linkage::Local, false)?;

        self.types
            .associated_functions
            .entry(owner)
            .or_default()
            .insert(decl.name.clone(), symbol);

        Ok(())
    }

    // Build a function's body. The function must already be pre-declared in scope via pre_declare_fn.
    fn build_fn_body_with_symbol(
        &mut self,
        decl: &FunctionDecl,
        func_id: FuncId,
        func_type: FuncType,
        span: &SimpleSpan,
        main_implicit_zero: bool,
    ) -> Result<(), ManyErrors> {
        if decl.r#extern {
            return Ok(());
        }

        let FunctionDecl { params, body, .. } = decl;

        // Reconstruct fn_params from decl names + func_type resolved types.
        let mut fn_params: Vec<(String, Type, ArgumentPurpose)> = params
            .iter()
            .zip(func_type.params.iter())
            .map(|(param, param_type)| {
                // is_extern is always false here, so purpose is always Normal
                (
                    param.name.0.clone(),
                    param_type.clone(),
                    ArgumentPurpose::Normal,
                )
            })
            .collect();

        // Add implicit @return_addr parameter for struct return values.
        if let Type::Struct(_) = &*func_type.return_type {
            fn_params.push((
                "@return_addr".to_owned(),
                (*func_type.return_type).clone(),
                ArgumentPurpose::StructReturn,
            ));
        }

        // Reconstruct the signature (must match what was declared in pre_declare_fn).
        let returns = match &*func_type.return_type {
            Type::Void | Type::Never | Type::Struct(_) => Vec::new(),
            return_type => vec![
                self.get_extern_abi_param(return_type)
                    .expect("Return type must be valid."),
            ],
        };
        let signature = Signature {
            call_conv: self.isa.default_call_conv(),
            params: fn_params
                .iter()
                .map(|(_, r#type, purpose)| {
                    let mut abi_param: AbiParam = self
                        .get_abi_param(r#type)
                        .expect("Param type must be valid.");
                    abi_param.purpose = *purpose;
                    abi_param
                })
                .collect(),
            returns,
        };

        let mut fn_context = Context::new();
        let mut fn_builder_context = FunctionBuilderContext::new();
        let mut fn_builder = FunctionBuilder::new(&mut fn_context.func, &mut fn_builder_context);

        fn_builder.func.signature = signature;

        let entry_block = fn_builder.create_block();
        fn_builder.append_block_params_for_function_params(entry_block);
        fn_builder.switch_to_block(entry_block);
        fn_builder.seal_block(entry_block);

        let mut errors = ManyErrors::new();

        // Move current scope into a child function scope.
        let outer_scope = std::mem::replace(&mut self.current_scope, Scope::new());
        self.current_scope = outer_scope.function_scope();

        let mut fn_generator = FnGenerator {
            state: self,
            builder: fn_builder,
            var_count: 0,
            current_func_type: func_type.clone(),
            return_dest: None,
        };

        for (i, (param_name, param_type, purpose)) in fn_params.iter().enumerate() {
            let param_value = fn_generator.builder.block_params(entry_block)[i];

            let var_type = fn_generator
                .state
                .get_abi_param(param_type)
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
            Ok(false) if main_implicit_zero => {
                let zero = fn_generator.builder.ins().iconst(types::I32, 0);
                fn_generator.builder.ins().return_(&[zero]);
            }
            Ok(false) => errors.push(CodeError::MissingReturnStatement.spanned(*span)),
            Ok(true) => {}
        }

        if fn_generator.state.verbose {
            println!("{}", fn_generator.builder.func.display());
        }

        // fn_generator dropped here, releasing the &mut self borrow.
        drop(fn_generator);

        // Restore the outer scope from the function scope's parent.
        self.current_scope = *self
            .current_scope
            .parent
            .take()
            .expect("fn scope must have parent");

        if !errors.is_empty() {
            return Err(errors);
        }

        self.module
            .define_function(func_id, &mut fn_context)
            .unwrap();

        Ok(())
    }

    fn build_fn_body(
        &mut self,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        is_top_level: bool,
    ) -> Result<(), ManyErrors> {
        let (func_id, func_type) = self
            .current_scope
            .get_function(&decl.name)
            .map(|(id, ft)| (id, ft.clone()))
            .expect("Function must be pre-declared before building its body");

        let main_implicit_zero =
            is_top_level && !decl.r#extern && decl.name == "main" && decl.return_type.is_none();

        self.build_fn_body_with_symbol(decl, func_id, func_type, span, main_implicit_zero)
    }

    fn build_assoc_fn_body(
        &mut self,
        owner: TypeId,
        decl: &FunctionDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        let Symbol::Function(func_id, func_type) = self
            .types
            .associated_functions
            .get(&owner)
            .and_then(|map| map.get(&decl.name))
            .cloned()
            .expect("Associated function must be pre-declared before building its body")
        else {
            unreachable!("associated function table must only contain functions");
        };

        self.build_fn_body_with_symbol(decl, func_id, func_type, span, false)
    }

    // Used for nested function declarations inside function bodies (single-pass is fine there).
    fn build_fn_decl(&mut self, decl: &FunctionDecl, span: &SimpleSpan) -> Result<(), ManyErrors> {
        self.pre_declare_fn(decl, span, false)?;
        self.build_fn_body(decl, span, false)
    }

    fn type_from_path(&self, path: &[Spanned<String>]) -> Option<Type> {
        if path.len() == 1 {
            return self.type_from_name(&path[0].0);
        }
        let LookupResult::Found(Symbol::Module(map)) = self.current_scope.get_symbol(&path[0].0)
        else {
            return None;
        };
        let mut map = map;
        for seg in &path[1..path.len() - 1] {
            match map.get(&seg.0)? {
                Symbol::Module(m) => map = m,
                _ => return None,
            }
        }
        match map.get(&path.last().unwrap().0)? {
            Symbol::Type(t) => Some(t.clone()),
            _ => None,
        }
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
            name => return self.current_scope.get_type(name).cloned(),
        })
    }
}

impl FnGenerator<'_, '_> {
    fn build_fn_call(
        &mut self,
        callee: &Spanned<Expression>,
        args: &[Spanned<Expression>],
        span: &SimpleSpan,
    ) -> Result<(Value, Type), ManyErrors> {
        let callee_name = expr_callee_name(&callee.0);

        let Some((func_id, func_type)) = (match self.resolve_expr_as_symbol(callee) {
            Some(Symbol::Function(id, ft)) => Some((*id, ft.clone())),
            _ => None,
        }) else {
            return CodeError::UnknownFunction(callee_name).err(*span);
        };

        if args.len() != func_type.params.len() {
            return CodeError::FunctionCallArgumentCount(
                callee_name,
                func_type.params.len(),
                args.len(),
            )
            .err(*span);
        }

        let arg_values = args
            .iter()
            .zip(func_type.params.iter())
            .map(|(expr, param_type)| {
                let value = self.build_expression(expr, Some(param_type.clone()))?;
                if !can_coerce_type(&value.1, param_type) {
                    return CodeError::FunctionCallArgumentType(
                        expr_callee_name(&callee.0),
                        param_type.to_string(),
                        value.1.to_string(),
                    )
                    .err(expr.1);
                }

                Ok(value)
            })
            .collect::<Result<Vec<(Value, Type)>, ManyErrors>>()?;

        if arg_values.iter().any(|(_, ty)| *ty == Type::Never) {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
        }

        if arg_values.iter().any(|(_, ty)| *ty == Type::Error) {
            return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
        }

        let mut arg_values = arg_values
            .into_iter()
            .map(|(value, _)| value)
            .collect::<Vec<_>>();

        if let Type::Struct(struct_id) = &*func_type.return_type {
            let (size, align_shift) = self.state.types.layout(&Type::Struct(*struct_id));
            if func_type.is_extern {
                todo!("Extern return structs.");
            }
            let stack_slot_data =
                StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift);
            let stack_slot = self.builder.create_sized_stack_slot(stack_slot_data);

            let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
            arg_values.push(struct_addr);
        }

        let func_ref = self
            .state
            .module
            .declare_func_in_func(func_id, self.builder.func);
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
}

impl GeneratorState<'_> {
    fn build_struct_decl(&mut self, decl: &StructDecl) -> Result<(), ManyErrors> {
        let mut errors = ManyErrors::new();

        if matches!(
            self.current_scope.get_symbol(&decl.name.0),
            LookupResult::Found(_)
        ) {
            errors.push(CodeError::NameAlreadyDefined(decl.name.0.clone()).spanned(decl.name.1));
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
            let (field_size, field_align_shift) = self.types.layout(&field_type);
            let field_align = 1i32 << field_align_shift;
            let padding = (field_align - (current_offset % field_align)) % field_align;
            current_offset += padding;
            fields.insert(
                name.0.clone(),
                StructField {
                    r#type: field_type,
                    offset: current_offset,
                },
            );
            current_offset += field_size as i32;
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let name = QualifiedName(self.current_module, decl.name.0.clone());
        let struct_type = StructType::new(name, fields, &self.types);
        let struct_id = self.types.alloc_type_id(self.current_module);

        if self.verbose {
            println!("Defined struct: \n {struct_type:?}");
        }
        self.types.structs.insert(struct_id, struct_type);
        self.current_scope
            .insert_symbol(decl.name.0.clone(), Symbol::Type(Type::Struct(struct_id)))
            .expect("Must not overwrite symbol");

        for func_decl in &decl.functions {
            if let Err(mut e) =
                self.pre_declare_assoc_fn(struct_id, &decl.name.0, &func_decl.0, &func_decl.1)
            {
                errors.append(&mut e);
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        for func_decl in &decl.functions {
            if let Err(mut e) = self.build_assoc_fn_body(struct_id, &func_decl.0, &func_decl.1) {
                errors.append(&mut e);
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(())
    }

    fn build_enum_decl(&mut self, decl: &EnumDecl) -> Result<(), ManyErrors> {
        let mut errors = ManyErrors::new();

        if matches!(
            self.current_scope.get_symbol(&decl.name.0),
            LookupResult::Found(_)
        ) {
            errors.push(CodeError::NameAlreadyDefined(decl.name.0.clone()).spanned(decl.name.1));
        }

        let mut variants = IndexSet::new();
        for name in &decl.variants {
            if variants.contains(&name.0) {
                errors.push(CodeError::EnumDuplicateVariant(name.0.clone()).spanned(name.1));
                continue;
            }
            variants.insert(name.0.clone());
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let enum_type = EnumType {
            name: QualifiedName(self.current_module, decl.name.0.clone()),
            variants,
        };
        let enum_id = self.types.alloc_type_id(self.current_module);

        if self.verbose {
            println!("Defined enum: \n {enum_type:?}");
        }

        self.types.enums.insert(enum_id, enum_type);
        self.current_scope
            .insert_symbol(decl.name.0.clone(), Symbol::Type(Type::Enum(enum_id)))
            .expect("Must not overwrite symbol");

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
            Type::Enum(_) => types::I32,
            Type::Struct(struct_id) => {
                let struct_type = &self.types.structs[struct_id];
                // StructArgument must have a size that's a multiple of 8 byte
                purpose = ArgumentPurpose::StructArgument(struct_type.size_pad_8());
                match struct_type.size {
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
            Type::Enum(_) => types::I32,
            _ => return None,
        })
    }
}

fn expr_callee_name(expr: &Expression) -> String {
    match expr {
        Expression::Identifier(name) => name.clone(),
        Expression::MemberAccess(base, member) => {
            format!("{}.{}", expr_callee_name(&base.0), member.0)
        }
        _ => "<expression>".to_string(),
    }
}

fn combine_spans(a: SimpleSpan, b: SimpleSpan) -> SimpleSpan {
    SimpleSpan::new(a.context, a.start..b.end)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use luga_syntax::parse_source;

    use super::*;

    fn generation_errors(src: &str) -> ManyErrors {
        let path = Path::new("test.luga");
        let program = parse_source(src, false)
            .unwrap_or_else(|err| panic!("test source must parse: {}", err.short_reason()));

        match generate(program, Some(path), src, false, false) {
            Err(GeneratorErrors::ModuleErrors(modules)) => modules
                .into_iter()
                .flat_map(|module| module.errors)
                .filter_map(|error| match error {
                    CompileError::Code(error) => Some(error),
                    _ => None,
                })
                .collect(),
            Err(GeneratorErrors::VerifierErrors(errors)) => {
                panic!("expected code errors, got verifier errors: {errors:?}")
            }
            Err(GeneratorErrors::MissingMain) => panic!("expected code errors, got missing main"),
            Ok(_) => panic!("expected code errors"),
        }
    }

    fn has_unreachable(errors: &ManyErrors) -> bool {
        errors
            .iter()
            .any(|Spanned(error, _)| matches!(error, CodeError::UnreachableStatement))
    }

    #[test]
    fn poisoned_return_expression_does_not_make_following_statement_unreachable() {
        let errors = generation_errors(
            r#"
fn main() {
    let bad = "x" + 1;
    return bad;
    @println("after");
}
"#,
        );

        assert!(!has_unreachable(&errors));
    }

    #[test]
    fn poisoned_if_condition_does_not_make_else_branch_prove_return() {
        let errors = generation_errors(
            r#"
fn main() {
    let bad = "x" + 1;
    if (bad) {
        @println("then");
    } else {
        return 0;
    }
    @println("after");
}
"#,
        );

        assert!(!has_unreachable(&errors));
    }
}
