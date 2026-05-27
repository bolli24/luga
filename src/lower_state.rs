use crate::lower::{MirLowerer, ModuleId};
use crate::lower_scope::{LookupResult, LowerScope, LowerSymbol};
use crate::mir::{MirFunction, MirProgram};
use crate::types::{
    EnumType, FuncType, QualifiedName, StructField, StructType, Type, TypeContext, TypeId,
};
use crate::{CodeError, CompileError, LowerErrors, MainModuleError, ManyErrors, ModuleErrors};
use indexmap::{IndexMap, IndexSet};
use luga_syntax::{
    Ast, EnumDecl, FunctionDecl, Param, SimpleSpan, Spanned, StructDecl, TopLevelItem, parse_source,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LowerFuncId(usize);

pub struct LowerState {
    current_module: ModuleId,
    pub(crate) types: TypeContext,
    pub(crate) current_scope: LowerScope,
    fn_name_prefix: String,
    functions: Vec<FuncType>,
    imported_mir_functions: Vec<MirFunction>,
    dir: PathBuf,
    compiling_imports: HashSet<PathBuf>,
    compiled_imports: HashMap<PathBuf, HashMap<String, LowerSymbol>>,
    verbose: bool,
}

pub fn lower_ast(
    ast: Ast,
    file_path: Option<&Path>,
    src: &str,
    verbose: bool,
) -> Result<MirProgram, LowerErrors> {
    let mut state = LowerState {
        current_module: ModuleId(0),
        types: TypeContext::new(),
        current_scope: LowerScope::new(),
        fn_name_prefix: "".to_string(),
        functions: Vec::new(),
        imported_mir_functions: Vec::new(),
        dir: file_path
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        compiling_imports: HashSet::new(),
        compiled_imports: HashMap::new(),
        verbose,
    };

    let mut mod_errors = Vec::new();

    let mut mir_functions = match state.lower_main_module(&ast, &mut mod_errors) {
        Err(MainModuleError::MissingMain) => return Err(LowerErrors::MissingMain),
        Err(MainModuleError::CodeErrors(errors)) => {
            mod_errors.push(ModuleErrors {
                path: file_path.map(Path::to_path_buf),
                src: src.to_owned(),
                errors: errors
                    .into_iter()
                    .map(|err| CompileError::Code(err))
                    .collect(),
            });
            return Err(LowerErrors::ModuleErrors(mod_errors));
        }
        Ok(mir_functions) => mir_functions,
    };

    let mut all_mir_functions = state.imported_mir_functions;
    all_mir_functions.append(&mut mir_functions);

    Ok(MirProgram(all_mir_functions))
}

impl LowerState {
    fn lower_main_module(
        &mut self,
        ast: &Ast,
        mod_errors: &mut Vec<ModuleErrors>,
    ) -> Result<Vec<MirFunction>, MainModuleError> {
        let found_main = self
            .pass1(&ast, mod_errors, true)
            .map_err(|errors| MainModuleError::CodeErrors(errors))?;

        if !found_main {
            return Err(MainModuleError::MissingMain);
        }

        let mut errors = ManyErrors::new();

        let mir_functions = self.lower_mir_functions(&ast, &mut errors);

        if !errors.is_empty() {
            return Err(MainModuleError::CodeErrors(errors));
        }
        Ok(mir_functions)
    }

    fn lower_mir_functions(&mut self, program: &Ast, errors: &mut ManyErrors) -> Vec<MirFunction> {
        let mut functions = Vec::new();

        for item in &program.items {
            match &item.0 {
                TopLevelItem::FunctionDecl(decl) => {
                    if decl.r#extern {
                        continue;
                    }

                    let Some((_, func_type)) = self.current_scope.get_function(&decl.name) else {
                        continue;
                    };
                    let func_type = func_type.clone();

                    let outer_scope = std::mem::replace(&mut self.current_scope, LowerScope::new());
                    self.current_scope = outer_scope.function_scope();

                    let lowerer = MirLowerer::new(self, func_type);
                    let result = lowerer.lower_function(decl, &item.1, true);

                    self.current_scope = *self
                        .current_scope
                        .parent
                        .take()
                        .expect("mir function scope must have parent");

                    match result {
                        Ok(mut lowered_functions) => functions.append(&mut lowered_functions),
                        Err(mut new_errors) => errors.append(&mut new_errors),
                    }
                }
                TopLevelItem::StructDecl(decl) => {
                    let LookupResult::Found(LowerSymbol::Type(Type::Struct(struct_id))) =
                        self.current_scope.get_symbol(&decl.name.0)
                    else {
                        continue;
                    };
                    let struct_id = *struct_id;

                    let lowerer = MirLowerer::new(
                        self,
                        FuncType {
                            name: String::new(),
                            params: Vec::new(),
                            return_type: Type::Void.into(),
                            is_extern: false,
                        },
                    );
                    let mut lowerer = lowerer;

                    for func_decl in &decl.functions {
                        if let Err(mut new_errors) =
                            lowerer.lower_associated_function(struct_id, &func_decl.0, &func_decl.1)
                        {
                            errors.append(&mut new_errors);
                        }
                    }

                    functions.append(&mut lowerer.into_nested_functions());
                }
                _ => {}
            }
        }

        functions
    }

    pub fn pass1(
        &mut self,
        program: &Ast,
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
                    if let Err(mut e) = self.declare_struct(decl) {
                        errors.append(&mut e);
                    }
                }
                TopLevelItem::EnumDecl(decl) => {
                    if let Err(mut e) = self.declare_enum(decl) {
                        errors.append(&mut e);
                    }
                }
                TopLevelItem::Import(path, rename) => {
                    match self.process_import(path, rename.as_ref(), item.1, mod_errors) {
                        Ok(()) => {}
                        Err(CompileError::Code(e)) => errors.push(e),
                        Err(CompileError::ModuleInner) => return Err(errors),
                        Err(CompileError::Lex(_) | CompileError::Parse(_)) => {
                            unreachable!("imports report parse errors through ModuleInner")
                        }
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

    pub fn pre_declare_fn(
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

        let symbol = self.make_function_symbol(decl, span, is_top_level)?;

        self.current_scope
            .insert_symbol(decl.name.clone(), symbol)
            .expect("Must not overwrite symbol");

        Ok(())
    }

    fn pre_declare_assoc_fn(
        &mut self,
        owner: TypeId,
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

        let LowerSymbol::Function(f_id, f_type) = self.make_function_symbol(decl, span, false)?
        else {
            unreachable!()
        };
        let symbol = LowerSymbol::Function(f_id, f_type);

        self.types
            .associated_functions
            .entry(owner)
            .or_default()
            .insert(decl.name.clone(), symbol);

        Ok(())
    }

    fn make_function_symbol(
        &mut self,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        is_top_level: bool,
    ) -> Result<LowerSymbol, ManyErrors> {
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

        let mut fn_params: Vec<(String, Type)> = Vec::new();
        let mut errors = ManyErrors::new();
        let mut param_types = Vec::new();

        if is_main && !params.is_empty() {
            errors.push(CodeError::MainHasParams.spanned(*span));
        }

        for Param {
            name,
            type_name,
            is_ref,
        } in params
        {
            let Some(r#type) = self.type_from_name(&type_name.0, *is_ref) else {
                errors.push(CodeError::UnknownType(type_name.0.clone()).spanned(type_name.1));
                continue;
            };

            // TODO: support extern ref params
            if *is_extern && *is_ref {
                errors.push(CodeError::ExternParamIsRef.spanned(name.1))
            }

            if fn_params
                .iter()
                .any(|(other_name, _)| &name.0 == other_name)
            {
                errors.push(CodeError::FunctionParamDuplicate(name.0.clone()).spanned(name.1));
            }

            param_types.push(r#type.clone());
            fn_params.push((name.0.clone(), r#type));
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let declared_return_type = match return_type {
            Some(type_name) => self
                .type_from_name(&type_name.0, false)
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

        let func_type = FuncType {
            name: name.to_owned(),
            params: param_types,
            return_type: return_type.into(),
            is_extern: *is_extern,
        };

        if self.verbose {
            println!("{func_type}");
        }

        let func_id = LowerFuncId(self.functions.len());
        self.functions.push(func_type.clone());

        Ok(LowerSymbol::Function(func_id, func_type))
    }

    pub fn declare_struct(&mut self, decl: &StructDecl) -> Result<TypeId, ManyErrors> {
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

            let Some(field_type) = self.type_from_name(&type_name.0, false) else {
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
            .insert_symbol(
                decl.name.0.clone(),
                LowerSymbol::Type(Type::Struct(struct_id)),
            )
            .expect("Must not overwrite symbol");

        for func_decl in &decl.functions {
            if let Err(mut e) = self.pre_declare_assoc_fn(struct_id, &func_decl.0, &func_decl.1) {
                errors.append(&mut e);
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(struct_id)
    }

    pub fn declare_enum(&mut self, decl: &EnumDecl) -> Result<TypeId, ManyErrors> {
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
            .insert_symbol(decl.name.0.clone(), LowerSymbol::Type(Type::Enum(enum_id)))
            .expect("Must not overwrite symbol");

        Ok(enum_id)
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
            let src = std::fs::read_to_string(&canonical).map_err(|err| {
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
                    self.compiling_imports.remove(&canonical);
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
            .insert_symbol(module_name.clone(), LowerSymbol::Module(symbols))
            .map_err(|_| {
                CodeError::NameAlreadyDefined(module_name)
                    .spanned(module_name_span)
                    .into()
            })
    }

    fn compile_import(
        &mut self,
        program: Ast,
        dir: &Path,
        canonical: &Path,
        mod_errors: &mut Vec<ModuleErrors>,
    ) -> Result<HashMap<String, LowerSymbol>, ManyErrors> {
        let new_module = ModuleId(self.current_module.0 + 1);
        let saved_module = std::mem::replace(&mut self.current_module, new_module);
        let saved_scope = std::mem::replace(&mut self.current_scope, LowerScope::new());
        let saved_dir = std::mem::replace(&mut self.dir, dir.to_path_buf());
        let new_prefix = module_prefix(canonical, self.compiled_imports.len());
        let saved_prefix = std::mem::replace(&mut self.fn_name_prefix, new_prefix);

        if let Err(errors) = self.pass1(&program, mod_errors, false) {
            self.current_module = saved_module;
            self.current_scope = saved_scope;
            self.dir = saved_dir;
            self.fn_name_prefix = saved_prefix;
            return Err(errors);
        }

        let mut errors = ManyErrors::new();
        let mut mir_functions = self.lower_mir_functions(&program, &mut errors);
        if !errors.is_empty() {
            self.current_module = saved_module;
            self.current_scope = saved_scope;
            self.dir = saved_dir;
            self.fn_name_prefix = saved_prefix;
            return Err(errors);
        }

        self.imported_mir_functions.append(&mut mir_functions);

        let module_symbols = std::mem::replace(&mut self.current_scope, saved_scope).symbols;
        self.current_module = saved_module;
        self.dir = saved_dir;
        self.fn_name_prefix = saved_prefix;

        Ok(module_symbols)
    }

    pub fn push_scope(&mut self) {
        let old_scope = std::mem::replace(&mut self.current_scope, LowerScope::new());
        self.current_scope.parent = Some(Box::new(old_scope));
    }

    pub fn pop_scope(&mut self) -> Option<LowerScope> {
        if let Some(mut parent) = self.current_scope.parent.take() {
            std::mem::swap(&mut self.current_scope, &mut *parent);
            Some(*parent)
        } else {
            None
        }
    }

    fn type_from_name(&self, type_name: &str, is_ref: bool) -> Option<Type> {
        let r#type = match type_name {
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
            name => self.current_scope.get_type(name).cloned()?,
        };
        Some(if is_ref {
            Type::Ref(Box::new(r#type))
        } else {
            r#type
        })
    }
}

pub fn module_prefix(canonical_path: &Path, module_index: usize) -> String {
    let stem = canonical_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}{module_index}$")
}
