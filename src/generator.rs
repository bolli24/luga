use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use cranelift::codegen::Context;
use cranelift::{
    codegen::ir::{ArgumentPurpose, StackSlot},
    module::{DataDescription, FuncId, Linkage, Module, default_libcall_names},
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{isa::CallConv, *},
};
use indexmap::IndexMap;
use luga_syntax::parser::{BinOp, Intrinsic, UnOp};

use crate::errors::*;
use crate::lower::ModuleId;
use crate::lower_state::LowerFuncId;
use crate::mir::{
    BasicBlock, FieldId, LocalId, MirCallee, MirConst, MirFunction, MirProgram, MirStatement,
    MirTerminator, Operand, Place as MirPlace, Rvalue,
};
use crate::types::{Type, *};

const UNREACHABLE: TrapCode = TrapCode::unwrap_user(1);

pub fn can_coerce_type(actual: &Type, expected: &Type) -> bool {
    matches!(actual, Type::Never | Type::Error)
        || actual == expected
        || matches!(actual, Type::Ref(inner) if inner.as_ref() == expected)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprValue {
    Slot(StackSlot, Type),
    Addr(Value, Type),
    Ssa(Value, Type),
    Never,
    Void,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GenValue {
    Ssa(Value, Type),
    Addr(Value, Type),
}

impl GenValue {
    pub fn ty(&self) -> &Type {
        match self {
            GenValue::Ssa(_, ty) | GenValue::Addr(_, ty) => ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalStore {
    Ssa(Value),
    Slot(StackSlot),
}

impl ExprValue {
    fn get_type(&self) -> Type {
        match self {
            ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty.clone(),
            ExprValue::Never => Type::Never,
            ExprValue::Void => Type::Void,
            ExprValue::Error => Type::Error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Place {
    addr: Value,
    ty: Type,
}

pub fn generate(
    program: MirProgram,
    file_path: Option<&Path>,
    release: bool,
    verbose: bool,
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

    let mut state = GeneratorState {
        isa,
        functions: &program.functions,
        function_types: &program.function_types,
        crane_functions: HashMap::new(),
        module: &mut module,
        current_module: ModuleId(0),
        strings: 0,
        printf_id,
        exit_id,
        dir: file_path
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        fn_name_prefix: String::new(),
        types: &program.types,
        verbose,
    };

    let crane_functions: HashMap<LowerFuncId, FuncId> = program
        .function_types
        .iter()
        .map(|(lower_func_id, func_type)| {
            (
                *lower_func_id,
                state.declare_function(func_type, *lower_func_id == program.main_func_id),
            )
        })
        .collect();

    state.crane_functions = crane_functions;

    state.build_program(&program)?;

    Ok(module.finish())
}

pub struct Scope {
    symbols: HashMap<String, Symbol>,
    pub(crate) parent: Option<Box<Scope>>,
    is_function_boundary: bool,
}

#[derive(Clone, Debug)]
pub enum Symbol {
    Variable(StackSlot, Type),
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

    pub(crate) fn contains_symbol_in_current_scope(&self, name: &str) -> bool {
        self.symbols.contains_key(name)
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

pub struct GeneratorState<'a> {
    isa: std::sync::Arc<dyn isa::TargetIsa>,
    functions: &'a IndexMap<LowerFuncId, MirFunction>,
    function_types: &'a IndexMap<LowerFuncId, FuncType>,
    crane_functions: HashMap<LowerFuncId, FuncId>,
    module: &'a mut ObjectModule,
    current_module: ModuleId,
    strings: usize,
    printf_id: FuncId,
    exit_id: FuncId,
    dir: PathBuf,
    fn_name_prefix: String,
    types: &'a TypeContext,
    verbose: bool,
}

impl<'a> GeneratorState<'a> {
    pub fn build_program(&mut self, program: &MirProgram) -> Result<(), GeneratorErrors> {
        for (lower_func_id, function) in &program.functions {
            self.build_fn_body(*lower_func_id, function)?;
        }

        Ok(())
    }

    fn declare_function(&mut self, func_type: &FuncType, is_main: bool) -> FuncId {
        let linkage = if func_type.is_extern {
            Linkage::Import
        } else if is_main {
            Linkage::Export
        } else {
            Linkage::Local
        };

        let mut params: Vec<AbiParam> = func_type
            .params
            .iter()
            .map(|param_type| {
                // TODO: support extern ref params
                debug_assert!(!(func_type.is_extern && param_type.is_ref()));

                let mut abi_param: AbiParam = if func_type.is_extern {
                    self.get_extern_abi_param(param_type)
                } else {
                    self.get_abi_param(param_type)
                }
                .expect("Param type must be valid.");

                abi_param.purpose = if let Type::Struct(struct_id) = &param_type
                    && func_type.is_extern
                {
                    let struct_type = &self.types.structs[struct_id];
                    ArgumentPurpose::StructArgument(struct_type.size_pad_8())
                } else {
                    ArgumentPurpose::Normal
                };
                abi_param
            })
            .collect();

        let return_type = &func_type.return_type;
        if self.types.size(return_type) > 8 {
            todo!("Return values larger than 8 bytes.")
        }

        let returns = match return_type.as_ref() {
            Type::Void | Type::Never => Vec::new(),

            Type::Struct(_) => {
                params.push(AbiParam::special(
                    self.isa.pointer_type(),
                    ArgumentPurpose::StructReturn,
                ));
                Vec::new()
            }

            return_type => vec![
                self.get_extern_abi_param(return_type)
                    .expect("Return type must be valid."),
            ],
        };

        let signature = Signature {
            call_conv: self.isa.default_call_conv(),
            params,
            returns,
        };

        self.module
            .declare_function(&func_type.name, linkage, &signature)
            .expect("Function must be valid")
    }
}

struct FnGenerator<'a, 'ctx> {
    state: &'ctx mut GeneratorState<'a>,
    builder: FunctionBuilder<'ctx>,
    var_count: usize,
    current_func_type: FuncType,
    return_dest: Option<StackSlot>,
    local_storage: HashMap<LocalId, StackSlot>,
    crane_blocks: Vec<Block>,
}

fn module_prefix(canonical_path: &Path, module_index: usize) -> String {
    let stem = canonical_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stem}{module_index}$")
}

impl<'a, 'ctx> FnGenerator<'a, 'ctx> {
    fn get_value(&mut self, value: ExprValue) -> Option<(Value, Type)> {
        match value {
            ExprValue::Ssa(value, ty) => Some((value, ty)),
            ExprValue::Addr(addr, ty @ Type::Struct(_)) => Some((addr, ty)),
            ExprValue::Addr(addr, ty) => {
                let crane_ty = self.state.get_crane_type(&ty)?;
                let value = self
                    .builder
                    .ins()
                    .load(crane_ty, MemFlags::trusted(), addr, 0);
                Some((value, ty))
            }
            ExprValue::Slot(slot, ty @ Type::Struct(_)) => {
                let addr = self
                    .builder
                    .ins()
                    .stack_addr(self.state.isa.pointer_type(), slot, 0);
                Some((addr, ty))
            }
            ExprValue::Slot(slot, ty) => {
                let crane_ty = self.state.get_crane_type(&ty)?;
                let addr = self
                    .builder
                    .ins()
                    .stack_addr(self.state.isa.pointer_type(), slot, 0);
                let value = self
                    .builder
                    .ins()
                    .load(crane_ty, MemFlags::trusted(), addr, 0);
                Some((value, ty))
            }
            ExprValue::Never | ExprValue::Void | ExprValue::Error => None,
        }
    }

    /*fn build_statements(
        &mut self,
        statements: &[Spanned<Statement>],
        try_into_type: Option<Type>,
    ) -> Result<ExprValue, ManyErrors> {
        let mut errors = ManyErrors::new();
        let mut iter = statements.iter().peekable();
        let mut block_value = ExprValue::Void;
        while let Some(statement) = iter.next() {
            let is_result = matches!(statement.0, Statement::Result(_));

            match self.build_statement(
                statement,
                if is_result {
                    try_into_type.clone()
                } else {
                    None
                },
            ) {
                Err(mut error) => {
                    errors.append(&mut error);
                }
                Ok(value @ ExprValue::Never) => {
                    if let Some(next) = iter.peek() {
                        if is_result {
                            errors.push(CodeError::ResultStatementNotLast.spanned(statement.1));
                        } else {
                            errors.push(CodeError::UnreachableStatement.spanned(next.1));
                        }
                    }
                    block_value = value;
                    break;
                }
                Ok(value) => {
                    if is_result && iter.peek().is_some() {
                        errors.push(CodeError::ResultStatementNotLast.spanned(statement.1));
                        block_value = value;
                        break;
                    }

                    block_value = value;
                }
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(block_value)
    }*/

    fn build_statement(&mut self, statement: &MirStatement) {
        if self.state.verbose {
            println!("statement: {statement}");
        }
        match statement {
            MirStatement::Assign(place, rvalue) => {
                let value = self.build_value(rvalue);
                self.build_assign(place, value);
            }
            MirStatement::Eval(Rvalue::Intrinsic { intrinsic, args }) => {
                self.build_eval_intrinsic(intrinsic, args);
            }
            MirStatement::Eval(Rvalue::Call { callee, args }) => {
                self.build_fn_call(callee, args);
            }
            MirStatement::Eval(rvalue) => {
                self.build_value(rvalue);
            }
        }
    }

    fn build_value(&mut self, rvalue: &Rvalue) -> GenValue {
        let value = match rvalue {
            Rvalue::Use(operand) => self.build_use_operand(operand),
            Rvalue::Binary { op, left, right } => self.build_binary_op(op, left, right),
            Rvalue::Unary { op, operand } => self.build_unary_op(op, operand),
            Rvalue::Call { callee, args } => self
                .build_fn_call(callee, args)
                .expect("function must return non-void value"),
            Rvalue::Intrinsic { intrinsic, args } => self.build_value_intrinsic(intrinsic, args),
            Rvalue::StructInit { ty, fields } => self.build_struct_init(*ty, fields),
            Rvalue::EnumVariant { ty, value } => GenValue::Ssa(
                self.builder.ins().iconst(types::I32, *value as i64),
                Type::Enum(*ty),
            ),
        };

        debug_assert!(!matches!(
            value.ty(),
            Type::Void | Type::Never | Type::Error
        ));

        value
    }

    fn build_value_intrinsic(&mut self, intrinsic: &Intrinsic, args: &[Operand]) -> GenValue {
        let args = args
            .iter()
            .map(|arg| self.build_use_operand(arg))
            .collect::<Vec<_>>();

        match intrinsic {
            Intrinsic::Print | Intrinsic::Println => self.build_printf(args),
            Intrinsic::EnumToInt => self.build_enum_to_int(args),
            Intrinsic::Assert | Intrinsic::Exit => unreachable!("intrinsic cannot be rvalue"),
        }
    }

    fn build_eval_intrinsic(&mut self, intrinsic: &Intrinsic, args: &[Operand]) {
        match intrinsic {
            Intrinsic::Assert => {
                let args = args
                    .iter()
                    .map(|arg| self.build_use_operand(arg))
                    .collect::<Vec<_>>();
                self.build_assert(args);
            }
            Intrinsic::Exit => {
                let args = args
                    .iter()
                    .map(|arg| self.build_use_operand(arg))
                    .collect::<Vec<_>>();
                self.build_exit(args);
            }
            Intrinsic::Print | Intrinsic::Println | Intrinsic::EnumToInt => {
                let _ = self.build_value_intrinsic(intrinsic, args);
            }
        }
    }

    fn build_use_operand(&mut self, operand: &Operand) -> GenValue {
        match operand {
            Operand::Place(place) => self.build_place_load(place),
            Operand::Const(constant, ty) => match constant {
                MirConst::Bool(value) => GenValue::Ssa(
                    self.builder.ins().iconst(types::I8, i64::from(*value)),
                    Type::Bool,
                ),
                MirConst::Float(value) => {
                    GenValue::Ssa(self.builder.ins().f32const(*value), Type::F32)
                }
                MirConst::Integer(value) => GenValue::Ssa(
                    self.builder.ins().iconst(
                        self.state
                            .get_crane_type(ty)
                            .expect("Integer must have cranelift type"),
                        *value,
                    ),
                    ty.clone(),
                ),
                MirConst::String(value) => {
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

                    GenValue::Ssa(value, Type::String)
                }
            },
        }
    }

    fn build_place_load(&mut self, place: &MirPlace) -> GenValue {
        let place = self.build_place_addr(place);

        match &place.ty {
            Type::Struct(_) => GenValue::Addr(place.addr, place.ty),
            ty => {
                let clif_ty = self
                    .state
                    .get_crane_type(ty)
                    .expect("place type must have a Cranelift type");

                let value = self
                    .builder
                    .ins()
                    .load(clif_ty, MemFlags::trusted(), place.addr, 0);

                GenValue::Ssa(value, place.ty)
            }
        }
    }

    fn build_place_addr(&mut self, place: &MirPlace) -> Place {
        match place {
            MirPlace::Local(local_id, ty) => {
                let stack_slot = self
                    .local_storage
                    .get(local_id)
                    .expect("Stack slot must exist.");
                let addr = self.stack_slot_addr(*stack_slot);
                Place {
                    addr,
                    ty: ty.clone(),
                }
            }
            MirPlace::Field {
                base,
                field_id,
                ty: _,
            } => {
                let base_place = self.build_place_addr(base);

                let Type::Struct(struct_id) = &base_place.ty else {
                    unreachable!("field access only valid for struct types")
                };

                let (_, field) = self.state.types.structs[struct_id]
                    .fields
                    .get_index(field_id.0)
                    .expect("field must exist");

                let field_addr = self
                    .builder
                    .ins()
                    .iadd_imm(base_place.addr, field.offset as i64);

                Place {
                    addr: field_addr,
                    ty: field.ty.clone(),
                }
            }
            MirPlace::Deref { pointer, ty } => {
                let pointer_place = self.build_place_addr(pointer);
                let ptr = self.builder.ins().load(
                    self.state.isa.pointer_type(),
                    MemFlags::trusted(),
                    pointer_place.addr,
                    0,
                );

                Place {
                    addr: ptr,
                    ty: ty.clone(),
                }
            }
        }
    }

    fn abi_value(&mut self, value: GenValue) -> (Value, Type) {
        match value {
            GenValue::Ssa(value, ty) => (value, ty),
            GenValue::Addr(addr, ty @ Type::Struct(_)) => (addr, ty),
            GenValue::Addr(addr, ty) => {
                let clif_ty = self
                    .state
                    .get_crane_type(&ty)
                    .expect("address value must be loadable as scalar");

                let value = self
                    .builder
                    .ins()
                    .load(clif_ty, MemFlags::trusted(), addr, 0);

                (value, ty)
            }
        }
    }

    fn scalar_value(&mut self, value: GenValue) -> (Value, Type) {
        match value {
            GenValue::Ssa(value, ty) => (value, ty),
            GenValue::Addr(addr, ty) => {
                let clif_ty = self
                    .state
                    .get_crane_type(&ty)
                    .expect("address value must be loadable as scalar");

                let value = self
                    .builder
                    .ins()
                    .load(clif_ty, MemFlags::trusted(), addr, 0);

                (value, ty)
            }
        }
    }

    fn build_operand(&mut self, operand: &Operand) -> (Value, Type) {
        let value = self.build_use_operand(operand);
        self.abi_value(value)
    }

    fn build_scalar_operand(&mut self, operand: &Operand) -> (Value, Type) {
        let value = self.build_use_operand(operand);
        let (value, ty) = self.abi_value(value);

        debug_assert!(
            !matches!(
                ty,
                Type::Struct(_) | Type::Void | Type::Never | Type::Error | Type::Ref(_)
            ),
            "invalid scalar operand type {ty}"
        );

        (value, ty)
    }

    fn build_binary_op(&mut self, op: &BinOp, left: &Operand, right: &Operand) -> GenValue {
        let (left_value, left_type) = self.build_scalar_operand(left);
        let (right_value, right_type) = self.build_scalar_operand(right);
        debug_assert_eq!(left_type, right_type);

        let (value, ty) = match left_type {
            Type::I8 | Type::I32 | Type::I64 => {
                self.build_int_bin_op(*op, left_value, right_value, left_type)
            }
            Type::F32 | Type::F64 => {
                self.build_float_bin_op(*op, left_value, right_value, left_type)
            }
            Type::Bool => self.build_bool_bin_op(*op, left_value, right_value),
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
                    self.builder.ins().icmp(cc, left_value, right_value)
                };
                (result, Type::Bool)
            }
            _ => unreachable!(),
        };

        GenValue::Ssa(value, ty)
    }

    fn build_unary_op(&mut self, op: &UnOp, operand: &Operand) -> GenValue {
        let (value, ty) = self.build_scalar_operand(operand);

        if ty == Type::Bool && *op == UnOp::Not {
            let result = self.builder.ins().bxor_imm(value, 1);
            GenValue::Ssa(result, Type::Bool)
        } else {
            unreachable!();
        }
    }

    fn build_struct_init(
        &mut self,
        type_id: TypeId,
        field_operands: &[(FieldId, Operand)],
    ) -> GenValue {
        let field_values: Vec<_> = field_operands
            .iter()
            .map(|(field_id, operand)| (*field_id, self.build_use_operand(operand)))
            .collect();

        let struct_type = &self.state.types.structs[&type_id];
        let struct_size = struct_type.size;
        let struct_align_shift = struct_type.align_shift;
        let struct_fields = struct_type.fields.clone();
        debug_assert_eq!(struct_fields.len(), field_values.len());

        let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            struct_size,
            struct_align_shift,
        ));

        let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);

        for ((_, struct_field), (_, value)) in struct_fields.iter().zip(field_values) {
            let field_dest = self
                .builder
                .ins()
                .iadd_imm(struct_addr, struct_field.offset as i64);
            self.store_gen_value_to_addr(field_dest, &struct_field.ty, value);
        }

        GenValue::Addr(struct_addr, Type::Struct(type_id))
    }

    fn build_assign(&mut self, place: &MirPlace, value: GenValue) {
        let dst = self.build_place_addr(place).addr;
        let dst_ty = place.ty().clone();

        self.store_gen_value_to_addr(dst, &dst_ty, value);
    }

    fn build_call_arg(&mut self, arg: &Operand, param_ty: &Type) -> Value {
        match param_ty {
            Type::Ref(_) => {
                let Operand::Place(place) = arg else {
                    unreachable!("ref argument must be a place")
                };
                self.build_place_addr(place).addr
            }
            Type::Struct(_) => {
                let (value, ty) = self.build_operand(arg);
                debug_assert!(matches!(ty, Type::Struct(_)));
                value
            }
            _ => self.build_scalar_operand(arg).0,
        }
    }

    fn store_gen_value_to_addr(&mut self, dst: Value, dst_ty: &Type, value: GenValue) {
        match (dst_ty, value) {
            (Type::Struct(_), GenValue::Addr(src, src_ty)) => {
                debug_assert_eq!(&src_ty, dst_ty);
                self.copy_memory(src, dst, self.state.types.size(dst_ty));
            }
            (Type::Struct(_), GenValue::Ssa(_, src_ty)) => {
                unreachable!("struct value must be address-backed: {src_ty}")
            }
            (_, GenValue::Ssa(value, src_ty)) => {
                debug_assert_eq!(&src_ty, dst_ty);
                self.store_to_addr(dst, dst_ty, value);
            }
            (_, GenValue::Addr(src, ty)) => {
                debug_assert_eq!(&ty, dst_ty);
                let (value, _) = self.scalar_value(GenValue::Addr(src, ty));
                self.store_to_addr(dst, dst_ty, value);
            }
        }
    }

    fn copy_memory(&mut self, src: Value, dest: Value, size: u32) {
        // TODO: copy wider (word wide) chunks
        for offset in 0..size {
            let value = self
                .builder
                .ins()
                .load(types::I8, MemFlags::trusted(), src, offset as i32);
            self.builder
                .ins()
                .store(MemFlags::trusted(), value, dest, offset as i32);
        }
    }

    fn copy_struct_value(&mut self, struct_id: TypeId, src: Value, dest: Value) {
        let size = self.state.types.size(&Type::Struct(struct_id));
        self.copy_memory(src, dest, size);
    }

    fn new_stack_slot_for_local(&mut self, local_id: LocalId, ty: &Type) -> StackSlot {
        let (size, align_shift) = self.state.types.layout(ty);
        let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            align_shift,
        ));

        self.local_storage.insert(local_id, stack_slot);
        stack_slot
    }

    fn stack_slot_addr(&mut self, slot: StackSlot) -> Value {
        self.builder
            .ins()
            .stack_addr(self.state.isa.pointer_type(), slot, 0)
    }

    fn store_to_slot(&mut self, slot: StackSlot, ty: &Type, value: Value) {
        let addr = self.stack_slot_addr(slot);
        self.store_to_addr(addr, ty, value);
    }

    fn store_to_addr(&mut self, addr: Value, ty: &Type, value: Value) {
        if let Type::Struct(struct_id) = ty {
            self.copy_struct_value(*struct_id, value, addr);
        } else {
            self.builder
                .ins()
                .store(MemFlags::trusted(), value, addr, 0);
        }
    }

    /* fn read_slot(&mut self, slot: StackSlot, ty: Type) -> GenValue {
        debug_assert!(!matches!(ty, Type::Void | Type::Never | Type::Error));

        let addr = self.stack_slot_addr(slot);
        match ty {
            Type::Struct(_) => GenValue::Addr(addr, ty),
            ty => {
                let crane_ty = self
                    .state
                    .get_crane_type(&ty)
                    .expect("slot value type must have a Cranelift type");

                let value = self
                    .builder
                    .ins()
                    .load(crane_ty, MemFlags::trusted(), addr, 0);

                GenValue::Ssa(value, ty)
            }
        }
    }*/

    // Helper function for alignment
    fn align_to(offset: i32, align_shift: u8) -> i32 {
        let align = 1 << align_shift;
        (offset + align - 1) & !(align - 1)
    }

    /* fn build_return(&mut self, expr: &Spanned<Expression>) -> Result<bool, ManyErrors> {
        let value =
            self.build_expression(expr, Some(*self.current_func_type.return_type.clone()))?;
        self.build_return_value(value, expr.1)
    }

    fn build_return_value(
        &mut self,
        value: ExprValue,
        span: SimpleSpan,
    ) -> Result<bool, ManyErrors> {
        let ty = match &value {
            ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty.clone(),
            ExprValue::Error => return Ok(false),
            ExprValue::Never => return Ok(true),
            ExprValue::Void => Type::Void,
        };

        if !can_coerce_type(&ty, &self.current_func_type.return_type) {
            return CodeError::InvalidReturnType(
                self.current_func_type
                    .return_type
                    .display(&self.state.types)
                    .to_string(),
                ty.display(&self.state.types).to_string(),
            )
            .err(span);
        }

        match &ty {
            Type::Void => {
                self.builder.ins().return_(&[]);
            }
            Type::Struct(struct_id) => {
                let value = self
                    .get_value(value)
                    .expect("ssa or stack slot expression must produce a value");

                let return_dest = self
                    .return_dest
                    .expect("Return destination must be valid when returning struct.");

                let return_dest = self.stack_slot_addr(return_dest);
                let return_dest = self.builder.ins().load(
                    self.state.isa.pointer_type(),
                    MemFlags::trusted(),
                    return_dest,
                    0,
                );
                self.copy_struct_value(*struct_id, value.0, return_dest);

                self.builder.ins().return_(&[]);
            }
            _ => {
                let value = self
                    .get_value(value)
                    .expect("ssa or stack slot expression must produce a value");

                self.builder.ins().return_(&[value.0]);
            }
        }

        Ok(true)
    } */

    /*fn build_place_value(
        &mut self,
        expression: &Spanned<Expression>,
        _try_into_type: Option<Type>,
    ) -> Result<ExprValue, ManyErrors> {
        let Spanned(expression, span) = expression;
        match expression {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(Symbol::Variable(slot, var_type)) => {
                    Ok(self.read_slot(*slot, var_type.clone()))
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
                            return Ok(ExprValue::Ssa(val, Type::Enum(enum_id)));
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
                let expr_ty = match &expr_value {
                    ExprValue::Never => return Ok(ExprValue::Never),
                    ExprValue::Error => return Ok(ExprValue::Error),
                    ExprValue::Void => Type::Void,
                    ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => {
                        ty.clone()
                    }
                };
                let name = member_name;
                let field = match &expr_ty {
                    Type::Struct(struct_id) => self.state.types.structs[struct_id]
                        .fields
                        .get(&name.0)
                        .cloned(),
                    _ => None,
                }
                .ok_or_else(|| {
                    CodeError::UnknownField(expr_ty.to_string(), name.0.clone()).spanned(name.1)
                })?;

                let expr_value = self
                    .get_value(expr_value)
                    .expect("ssa or stack slot member base must produce a value");

                if matches!(field.ty, Type::Struct(_)) {
                    let field_addr = self
                        .builder
                        .ins()
                        .iadd_imm(expr_value.0, field.offset as i64);
                    return Ok(ExprValue::Addr(field_addr, field.ty));
                }

                let field_crane_type = self
                    .state
                    .get_crane_type(&field.ty)
                    .expect("Field type must be valid.");
                let struct_value = self.builder.ins().load(
                    field_crane_type,
                    MemFlags::new(),
                    expr_value.0,
                    field.offset,
                );

                Ok(ExprValue::Ssa(struct_value, field.ty))
            }
            _ => CodeError::ExpectedLValue.err(*span)?,
        }
    }*/

    /*fn build_place(&mut self, expression: &Spanned<Expression>) -> Result<Place, ManyErrors> {
        let Spanned(expression, span) = expression;
        match expression {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(Symbol::Variable(slot, ty)) => {
                    let slot = *slot;
                    let ty = ty.clone();
                    let addr = self.stack_slot_addr(slot);

                    if let Type::Ref(inner) = ty {
                        let ptr = self.builder.ins().load(
                            self.state.isa.pointer_type(),
                            MemFlags::trusted(),
                            addr,
                            0,
                        );
                        return Ok(Place {
                            addr: ptr,
                            ty: *inner,
                        });
                    }

                    Ok(Place { addr, ty })
                }
                // TODO: Add a dedicated diagnostic for non-addressable expressions used as places.
                LookupResult::Found(Symbol::Function(_, func_type)) => {
                    CodeError::FunctionUsedAsValue(func_type.name.clone()).err(*span)
                }
                LookupResult::Found(Symbol::Type(_)) => {
                    CodeError::TypeUsedAsValue(name.clone()).err(*span)
                }
                LookupResult::Found(Symbol::Module(_)) => {
                    CodeError::ModuleUsedAsValue(name.clone()).err(*span)
                }
                LookupResult::CaptureError => CodeError::Capture(name.clone()).err(*span),
                LookupResult::NotFound => CodeError::UnknownVariable(name.clone()).err(*span),
            },
            Expression::MemberAccess(base, member_name) => {
                let base_place = self.build_place(base)?;

                let Type::Struct(struct_id) = &base_place.ty else {
                    return CodeError::UnknownField(
                        base_place.ty.to_string(),
                        member_name.0.clone(),
                    )
                    .err(member_name.1);
                };

                let field = self.state.types.structs[struct_id]
                    .fields
                    .get(&member_name.0)
                    .cloned()
                    .ok_or_else(|| {
                        CodeError::UnknownField(base_place.ty.to_string(), member_name.0.clone())
                            .spanned(member_name.1)
                    })?;

                let field_addr = self
                    .builder
                    .ins()
                    .iadd_imm(base_place.addr, field.offset as i64);

                Ok(Place {
                    addr: field_addr,
                    ty: field.ty,
                })
            }
            _ => CodeError::ExpectedLValue.err(*span),
        }
    }*/

    /*fn build_expression(
        &mut self,
        expression: &Spanned<Expression>,
        try_into_type: Option<Type>,
    ) -> Result<ExprValue, ManyErrors> {
        let Spanned(expression, span) = expression;
        match expression {
            Expression::Float(value) => Ok(ExprValue::Ssa(
                self.builder.ins().f32const(*value as f32),
                Type::F32,
            )),
            Expression::Integer(value) => {
                let into_type = match try_into_type {
                    Some(ty) if ty.is_int() => ty,
                    _ => Type::I32,
                };

                let mut check_and_emit = |min: i64, max: i64, cranelift_type, out_type: Type| {
                    if *value < min || *value > max {
                        CodeError::CoerseInt(out_type.to_string(), *value).err(*span)
                    } else {
                        Ok(ExprValue::Ssa(
                            self.builder.ins().iconst(cranelift_type, *value),
                            out_type,
                        ))
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
                    Type::I64 => Ok(ExprValue::Ssa(
                        self.builder.ins().iconst(types::I64, *value),
                        Type::I64,
                    )),
                    Type::U64 => {
                        if *value < 0 {
                            CodeError::CoerseInt(Type::U64.to_string(), *value).err(*span)
                        } else {
                            Ok(ExprValue::Ssa(
                                self.builder.ins().iconst(types::I64, *value),
                                Type::U64,
                            ))
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

                Ok(ExprValue::Ssa(value, Type::String))
            }
            Expression::Bool(value) => Ok(ExprValue::Ssa(
                self.builder.ins().iconst(types::I8, i64::from(*value)),
                Type::Bool,
            )),
            Expression::Block(statements) => {
                return self.build_block_body(statements, try_into_type);
            }
            Expression::If(r#if) => {
                return self.build_if(r#if, *span, try_into_type);
            }
            Expression::Binary(op, x, y) => {
                let x_value = self.build_expression(x.as_ref(), try_into_type.clone())?;
                let y_value = self.build_expression(y.as_ref(), try_into_type)?;

                let x_ty = match &x_value {
                    ExprValue::Never => return Ok(ExprValue::Never),
                    ExprValue::Error => return Ok(ExprValue::Error),
                    ExprValue::Void => {
                        return CodeError::InvalidBinOpType(*op, Type::Void.to_string()).err(*span);
                    }
                    ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty,
                };

                let y_ty = match &y_value {
                    ExprValue::Never => return Ok(ExprValue::Never),
                    ExprValue::Error => return Ok(ExprValue::Error),
                    ExprValue::Void => {
                        return CodeError::InvalidBinOpType(*op, Type::Void.to_string()).err(*span);
                    }
                    ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty,
                };

                if x_ty != y_ty {
                    return CodeError::IncompatipleBinOpTypes(
                        *op,
                        x_ty.to_string(),
                        y_ty.to_string(),
                    )
                    .err(*span);
                }

                let x_ty = x_ty.clone();
                let y_ty_for_error = y_ty.to_string();

                let x_value = self
                    .get_value(x_value)
                    .expect("ssa or stack slot binary lhs must produce a value");
                let y_value = self
                    .get_value(y_value)
                    .expect("ssa or stack slot binary rhs must produce a value");

                let result = match x_ty {
                    Type::I8 | Type::I32 | Type::I64 => self
                        .build_int_bin_op(*op, x_value.0, y_value.0, x_ty)
                        .map(|(value, ty)| ExprValue::Ssa(value, ty)),
                    Type::F32 | Type::F64 => self
                        .build_float_bin_op(*op, x_value.0, y_value.0, x_ty)
                        .map(|(value, ty)| ExprValue::Ssa(value, ty)),
                    Type::Bool => self
                        .build_bool_bin_op(*op, x_value.0, y_value.0)
                        .map(|(value, ty)| ExprValue::Ssa(value, ty)),
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
                        Some(ExprValue::Ssa(result, Type::Bool))
                    }
                    _ => None,
                }
                .ok_or_else(|| CodeError::InvalidBinOpType(*op, y_ty_for_error).spanned(*span))?;

                Ok(result)
            }
            Expression::Unary(op, expr) => {
                let value = self.build_expression(expr.as_ref(), try_into_type)?;

                let ty = match &value {
                    ExprValue::Never => return Ok(ExprValue::Never),
                    ExprValue::Error => return Ok(ExprValue::Error),
                    ExprValue::Void => &Type::Void,
                    ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty,
                };

                if ty == &Type::Bool && *op == UnOp::Not {
                    let value = self
                        .get_value(value)
                        .expect("ssa or stack slot unary operand must produce a value");
                    let result = self.builder.ins().bxor_imm(value.0, 1);
                    Ok(ExprValue::Ssa(result, Type::Bool))
                } else {
                    CodeError::InvalidUnOpType(*op, ty.to_string()).err(*span)
                }
            }
            Expression::Intrinsic(intrinsic, args) => match intrinsic {
                Intrinsic::Print => self.build_printf(span, args, false),
                Intrinsic::Println => self.build_printf(span, args, true),
                Intrinsic::Assert => self.build_assert(span, args),
                Intrinsic::Exit => self.build_exit(span, args),
                Intrinsic::EnumToInt => self.build_enum_to_int(span, args),
            },
            Expression::Identifier(_) => {
                self.build_place_value(&Spanned(expression.clone(), *span), try_into_type)
            }
            Expression::FunctionCall(name, args) => self.build_fn_call_old(name, args, span),
            Expression::MemberAccess(_, _) => {
                self.build_place_value(&Spanned(expression.clone(), *span), try_into_type)
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
                        .map(|struct_field| struct_field.ty);

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
                let struct_size = struct_type.size;
                let struct_align_shift = struct_type.align_shift;
                let struct_fields = struct_type.fields.clone();

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    struct_size,
                    struct_align_shift,
                ));

                let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
                let mut poison_value = None;

                for (name, field) in &struct_fields {
                    let Some(value) = arg_values.remove(name) else {
                        errors.push(CodeError::StructInitMissingArg(name.clone()).spanned(*span));
                        continue;
                    };

                    let ty = match &value {
                        ExprValue::Never => {
                            poison_value = Some(ExprValue::Never);
                            continue;
                        }
                        ExprValue::Error => {
                            poison_value = Some(ExprValue::Error);
                            continue;
                        }
                        ExprValue::Void => &Type::Void,
                        ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => {
                            ty
                        }
                    };

                    if !can_coerce_type(ty, &field.ty) {
                        errors.push(
                            CodeError::StructInitInvalidType(field.ty.to_string(), ty.to_string())
                                .spanned(*span),
                        );
                        continue;
                    }

                    let value = self
                        .get_value(value)
                        .expect("ssa or stack slot struct field value must produce a value");

                    if let Type::Struct(field_struct_id) = field.ty {
                        let field_dest = self
                            .builder
                            .ins()
                            .iadd_imm(struct_addr, field.offset as i64);
                        self.copy_struct_value(field_struct_id, value.0, field_dest);
                    } else {
                        self.builder.ins().store(
                            MemFlags::trusted(),
                            value.0,
                            struct_addr,
                            field.offset,
                        );
                    }
                }

                if !errors.is_empty() {
                    return Err(errors);
                }

                if let Some(poison_value) = poison_value {
                    return Ok(poison_value);
                }

                Ok(ExprValue::Slot(stack_slot, r#type))
            }
        }
    }*/

    /*fn resolve_expr_as_symbol(&self, expr: &Spanned<Expression>) -> Option<&Symbol> {
        match &expr.0 {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(
                    sym @ (Symbol::Module(_) | Symbol::Type(_) | Symbol::Function(..)),
                ) => Some(sym),
                _ => None,
            },
            Expression::MemberAccess(base, member) => match self.resolve_expr_as_symbol(base)? {
                Symbol::Module(map) => map.get(&member.0),
                Symbol::Type(Type::Struct(_type_id) | Type::Enum(_type_id)) => unreachable!() /* self
                    .state
                    .types
                    .associated_functions
                    .get(type_id)?
                    .get(&member.0)*/,
                _ => None,
            },
            _ => None,
        }
    }*/

    /*fn resolve_method_receiver_type(&self, receiver_expr: &Spanned<Expression>) -> Option<Type> {
        match &receiver_expr.0 {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(Symbol::Variable(_, ty)) => Some(ty.clone()),
                _ => None,
            },
            Expression::MemberAccess(base, member) => {
                let base_type = self.resolve_method_receiver_type(base)?;
                let base_type = match &base_type {
                    Type::Ref(inner) => inner.as_ref(),
                    _ => &base_type,
                };

                match base_type {
                    Type::Struct(type_id) => self
                        .state
                        .types
                        .structs
                        .get(type_id)?
                        .fields
                        .get(&member.0)
                        .map(|field| field.ty.clone()),
                    _ => None,
                }
            }
            _ => None,
        }
    }*/

    /*fn resolve_method_call(
        &self,
        receiver_expr: &Spanned<Expression>,
        method_name: &str,
    ) -> Option<(FuncId, FuncType)> {
        let receiver_type = self.resolve_method_receiver_type(receiver_expr)?;
        let type_id = match &receiver_type {
            Type::Struct(type_id) | Type::Enum(type_id) => *type_id,
            Type::Ref(inner) => match inner.as_ref() {
                Type::Struct(type_id) | Type::Enum(type_id) => *type_id,
                _ => return None,
            },
            _ => return None,
        };

        match self
            .state
            .types
            .associated_functions
            .get(&type_id)?
            .get(method_name)?
        {
            LowerSymbol::Function(func_id, func_type) => {
                Some((unreachable!("*func_id"), func_type.clone()))
            }
            _ => None,
        }
    }*/

    #[must_use]
    fn build_int_bin_op(&mut self, op: BinOp, x: Value, y: Value, r#type: Type) -> (Value, Type) {
        if matches!(op, BinOp::Lt | BinOp::Gt | BinOp::Lte | BinOp::Gte) && r#type.is_unsigned_int()
        {
            todo!("Comparision of unsigned integer types.")
        };

        (
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
        )
    }

    #[must_use]
    fn build_float_bin_op(&mut self, op: BinOp, x: Value, y: Value, r#type: Type) -> (Value, Type) {
        (
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
        )
    }

    #[must_use]
    fn build_bool_bin_op(&mut self, op: BinOp, x: Value, y: Value) -> (Value, Type) {
        match op {
            BinOp::Equal => (self.builder.ins().icmp(IntCC::Equal, x, y), Type::Bool),
            BinOp::NotEqual => (self.builder.ins().icmp(IntCC::NotEqual, x, y), Type::Bool),
            _ => unreachable!(),
        }
    }

    fn build_printf(&mut self, args: Vec<GenValue>) -> GenValue {
        let Some((format_string_arg, other_args)) = args.split_first() else {
            unreachable!()
        };

        let format_string_value = self.abi_value(format_string_arg.clone());
        debug_assert_eq!(format_string_value.1, Type::String);

        let arg_values = other_args
            .iter()
            .cloned()
            .map(|arg| self.abi_value(arg))
            .collect::<Vec<_>>();

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
        GenValue::Ssa(value, Type::I32)
    }

    fn new_stack_slot(&mut self, local_id: LocalId, slot: StackSlot) {
        self.local_storage.insert(local_id, slot);
    }

    /*fn new_var(&mut self, name: String, slot: StackSlot, ty: Type) {
        self.state
            .current_scope
            .insert_symbol(name, Symbol::Variable(slot, ty))
            .expect("Variable must not exist.");
        self.var_count += 1;
    }*/

    /*fn new_error_var(&mut self, name: String) {
        let slot = self.new_stack_slot_for_local(&Type::I32);
        self.new_var(name, slot, Type::Error);
    }*/

    fn switch_to_recovery_block(&mut self) {
        let block = self.builder.create_block();
        self.builder.switch_to_block(block);
        self.builder.seal_block(block);
    }

    fn current_block_is_terminated(&self) -> bool {
        self.builder
            .current_block()
            .and_then(|block| self.builder.func.layout.last_inst(block))
            .is_some_and(|inst| self.builder.func.dfg.insts[inst].opcode().is_terminator())
    }

    fn trap_if_current_block_open(&mut self) {
        if !self.current_block_is_terminated() {
            self.builder.ins().trap(UNREACHABLE);
        }
    }

    /* // TODO: add clif comments to blocks
    fn build_if(
        &mut self,
        if_expr: &If,
        span: SimpleSpan,
        try_into_type: Option<Type>,
    ) -> Result<ExprValue, ManyErrors> {
        struct BranchExit {
            block: Block,
            r#type: Type,
        }

        let condition_blocks: Vec<_> = if_expr
            .if_branches
            .iter()
            .map(|_| self.builder.create_block())
            .collect();

        let else_block = if_expr
            .else_branch
            .as_ref()
            .map(|_| self.builder.create_block());

        let exit_block = self.builder.create_block();

        let mut errors = ManyErrors::new();

        self.builder.ins().jump(condition_blocks[0], []);

        let mut block_types = Vec::new();
        let mut branch_exits = Vec::new();
        let mut condition_poisoned = false;

        for (index, conditional) in if_expr.if_branches.iter().enumerate() {
            let condition_block = condition_blocks[index];
            let next_block: Block = condition_blocks
                .get(index + 1)
                .copied()
                .or(else_block)
                .unwrap_or(exit_block);

            self.builder.seal_block(condition_block);
            self.builder.switch_to_block(condition_block);
            // TODO: maybe set try_into_type to bool
            let condition_value = match self.build_expression(&conditional.condition, None) {
                Ok(ExprValue::Never) => {
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Ok(ExprValue::Error) => {
                    condition_poisoned = true;
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Ok(ExprValue::Void) => {
                    condition_poisoned = true;
                    errors.push(
                        CodeError::InvalidConditionType(Type::Void.to_string())
                            .spanned(conditional.condition.1),
                    );
                    self.builder.ins().jump(next_block, []);
                    continue;
                }
                Ok(condition_value) => {
                    let ty = match &condition_value {
                        ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => {
                            ty
                        }
                        ExprValue::Never | ExprValue::Error | ExprValue::Void => unreachable!(),
                    };

                    if !can_coerce_type(ty, &Type::Bool) {
                        condition_poisoned = true;
                        errors.push(
                            CodeError::InvalidConditionType(ty.to_string())
                                .spanned(conditional.condition.1),
                        );
                        self.builder.ins().jump(next_block, []);
                        continue;
                    }

                    self.get_value(condition_value)
                        .expect("ssa or stack slot condition must produce a value")
                }
                Err(mut err) => {
                    condition_poisoned = true;
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
            match self.build_block_body(&conditional.block, try_into_type.clone()) {
                Ok(ExprValue::Never) => {}
                Ok(ExprValue::Error) => {
                    self.trap_if_current_block_open();
                }
                Ok(block_value) => {
                    let block_type = block_value.get_type();
                    if block_type != Type::Never {
                        let branch_exit_block = self.builder.create_block();
                        let mut emitted_branch_exit = true;
                        if block_type == Type::Void {
                            self.builder.ins().jump(branch_exit_block, []);
                        } else if let Some(block_crane_type) =
                            self.state.get_crane_type(&block_type)
                        {
                            self.builder
                                .append_block_param(branch_exit_block, block_crane_type);
                            let block_value = self
                                .get_value(block_value)
                                .expect("ssa or stack slot branch result must produce a value");
                            self.builder
                                .ins()
                                .jump(branch_exit_block, [&block_value.0.into()]);
                        } else {
                            self.builder.ins().trap(UNREACHABLE);
                            emitted_branch_exit = false;
                        }
                        if emitted_branch_exit {
                            branch_exits.push(BranchExit {
                                block: branch_exit_block,
                                r#type: block_type.clone(),
                            });
                        }
                        block_types.push(block_type.clone());
                    }
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    self.trap_if_current_block_open();
                }
            }
        }

        if let Some(else_block) = else_block {
            self.builder.seal_block(else_block);
            self.builder.switch_to_block(else_block);

            // TODO: pass try_into_type
            match self.build_block_body(
                if_expr
                    .else_branch
                    .as_ref()
                    .expect("Else branch must exist."),
                try_into_type.clone(),
            ) {
                Ok(ExprValue::Never) => {}
                Ok(ExprValue::Error) => {
                    self.trap_if_current_block_open();
                }
                Ok(block_value) => {
                    let block_type = block_value.get_type();
                    if block_type != Type::Never {
                        let branch_exit_block = self.builder.create_block();
                        let mut emitted_branch_exit = true;
                        if block_type == Type::Void {
                            self.builder.ins().jump(branch_exit_block, []);
                        } else if let Some(block_crane_type) =
                            self.state.get_crane_type(&block_type)
                        {
                            self.builder
                                .append_block_param(branch_exit_block, block_crane_type);
                            let block_value = self
                                .get_value(block_value)
                                .expect("ssa or stack slot branch result must produce a value");
                            self.builder
                                .ins()
                                .jump(branch_exit_block, [&block_value.0.into()]);
                        } else {
                            self.builder.ins().trap(UNREACHABLE);
                            emitted_branch_exit = false;
                        }
                        if emitted_branch_exit {
                            branch_exits.push(BranchExit {
                                block: branch_exit_block,
                                r#type: block_type.clone(),
                            });
                        }
                        block_types.push(block_type.clone());
                    }
                }
                Err(mut err) => {
                    errors.append(&mut err);
                    self.trap_if_current_block_open();
                }
            }
        } else if try_into_type.is_some() || block_types.iter().any(|t| *t != Type::Void) {
            errors.push(CodeError::MissingElseBranch.spanned(span));
        }

        let expr_type = try_into_type
            .clone()
            .or_else(|| block_types.iter().find(|t| **t != Type::Void).cloned())
            .unwrap_or_else(|| {
                block_types.first().cloned().unwrap_or_else(|| {
                    if else_block.is_some() && !condition_poisoned {
                        Type::Never
                    } else {
                        Type::Void
                    }
                })
            });

        if block_types
            .iter()
            .any(|branch_type| !can_coerce_type(branch_type, &expr_type))
        {
            errors.push(CodeError::IfExpressionType.spanned(span));
        }

        if !errors.is_empty() {
            self.switch_to_recovery_block();
            return Err(errors);
        }

        if let Some(expr_crane_type) = self.state.get_crane_type(&expr_type) {
            self.builder.append_block_param(exit_block, expr_crane_type);
        }

        for branch_exit in branch_exits {
            self.builder.seal_block(branch_exit.block);
            self.builder.switch_to_block(branch_exit.block);
            if branch_exit.r#type == Type::Void {
                self.builder.ins().jump(exit_block, []);
            } else {
                let value = self.builder.block_params(branch_exit.block)[0];
                self.builder.ins().jump(exit_block, [&value.into()]);
            }
        }

        self.builder.seal_block(exit_block);
        self.builder.switch_to_block(exit_block);

        Ok(match expr_type {
            Type::Never => ExprValue::Never,
            Type::Void => ExprValue::Void,
            Type::Struct(_) => ExprValue::Addr(self.builder.block_params(exit_block)[0], expr_type),
            _ => ExprValue::Ssa(self.builder.block_params(exit_block)[0], expr_type),
        })
    }*/

    /* fn build_while(&mut self, conditional: &Conditional) -> Result<bool, ManyErrors> {
        let Conditional { condition, block } = conditional;

        let while_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        let condition_block = self.builder.create_block();
        self.builder.ins().jump(condition_block, []);
        self.builder.switch_to_block(condition_block);

        let condition_value = match self.build_expression(condition, None)? {
            ExprValue::Never | ExprValue::Error => {
                self.builder.ins().jump(exit_block, []);
                self.builder.switch_to_block(exit_block);
                self.builder.seal_block(condition_block);
                self.builder.seal_block(while_block);
                self.builder.seal_block(exit_block);
                return Ok(false);
            }
            ExprValue::Void => {
                return CodeError::InvalidConditionType(Type::Void.to_string()).err(condition.1);
            }
            condition_value => {
                let ty = match &condition_value {
                    ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty,
                    ExprValue::Never | ExprValue::Error | ExprValue::Void => unreachable!(),
                };

                if !can_coerce_type(ty, &Type::Bool) {
                    return CodeError::InvalidConditionType(ty.to_string()).err(condition.1);
                }

                self.get_value(condition_value)
                    .expect("ssa or stack slot condition must produce a value")
            }
        };

        self.builder
            .ins()
            .brif(condition_value.0, while_block, [], exit_block, []);

        self.builder.switch_to_block(while_block);

        self.build_block_body(block, None)?;

        self.builder.ins().jump(condition_block, []);

        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(condition_block);
        self.builder.seal_block(while_block);
        self.builder.seal_block(exit_block);

        Ok(false)
    }*/

    /* fn build_block_body(
        &mut self,
        body: &[Spanned<Statement>],
        try_into_type: Option<Type>,
    ) -> Result<ExprValue, ManyErrors> {
        self.state.push_scope();

        let result = self.build_statements(body, try_into_type);

        let Some(_) = self.state.pop_scope() else {
            unreachable!("Must always be able to pop scope after leaving block.");
        };

        result
    }*/

    fn build_assert(&mut self, mut args: Vec<GenValue>) {
        debug_assert_eq!(args.len(), 1);
        let value = self.scalar_value(args.pop().expect("must have value"));
        debug_assert_eq!(value.1, Type::Bool);

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
            .brif(value.0, continue_block, [], exit_block, []);

        self.builder.seal_block(exit_block);
        self.builder.seal_block(continue_block);

        self.builder.switch_to_block(exit_block);
        self.builder.ins().call(exit_ref, &[exit_code]);
        self.builder.ins().trap(UNREACHABLE);

        self.builder.switch_to_block(continue_block);
    }

    fn build_exit(&mut self, mut args: Vec<GenValue>) {
        debug_assert_eq!(args.len(), 1);
        let value = self.scalar_value(args.pop().expect("must have value"));
        debug_assert_eq!(value.1, Type::I32);

        let exit_ref = self
            .state
            .module
            .declare_func_in_func(self.state.exit_id, self.builder.func);

        self.builder.ins().call(exit_ref, &[value.0]);
    }

    fn build_enum_to_int(&mut self, mut args: Vec<GenValue>) -> GenValue {
        debug_assert_eq!(args.len(), 1);
        let value = self.scalar_value(args.pop().expect("must have value"));
        debug_assert!(matches!(value.1, Type::Enum(_)));

        GenValue::Ssa(value.0, Type::I32)
    }
}

impl GeneratorState<'_> {
    /* pub(crate) fn push_scope(&mut self) {
        let old_scope = std::mem::replace(&mut self.current_scope, Scope::new());
        self.current_scope.parent = Some(Box::new(old_scope));
    }

    pub(crate) fn pop_scope(&mut self) -> Option<Scope> {
        if let Some(mut parent) = self.current_scope.parent.take() {
            std::mem::swap(&mut self.current_scope, &mut *parent);
            Some(*parent)
        } else {
            None
        }
    }*/

    // Declare a function's signature in the module
    // Does not build the body — call build_fn_body separately for that.
    /*fn make_function_symbol(
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

        let returns = match &return_type {
            Type::Void | Type::Never => Vec::new(),
            Type::Struct(_) => {
                fn_params.push((
                    "@return_addr".to_owned(),
                    return_type.clone(), // TODO: can we just use Type::Ref(Box::new(return_type))?
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
                        debug_assert!(!r#type.is_ref());
                        self.get_extern_abi_param(r#type)
                    } else {
                        self.get_abi_param(r#type)
                    }
                    .expect("Param type must be valid.");
                    // If we ever support ref parameter in extern we must change the purpose for those params
                    debug_assert!(
                        !(*is_extern
                            && r#type.is_ref()
                            && matches!(purpose, ArgumentPurpose::StructArgument(_)))
                    );
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

        Ok(Symbol::Function(func_id, func_type))
    }*/

    /*pub(crate) fn pre_declare_fn(
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
    }*/

    /*fn pre_declare_assoc_fn(
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

        let Symbol::Function(f_id, f_type) =
            self.make_function_symbol(decl, span, &object_name, Linkage::Local, false)?
        else {
            unreachable!()
        };
        let symbol = LowerSymbol::Function(unreachable!("f_id"), f_type);

        self.types
            .associated_functions
            .entry(owner)
            .or_default()
            .insert(decl.name.clone(), symbol);

        Ok(())
    }*/

    fn build_fn_body(
        &mut self,
        lower_func_id: LowerFuncId,
        function: &MirFunction,
    ) -> Result<(), GeneratorErrors> {
        let mut fn_params: Vec<(LocalId, Type, ArgumentPurpose)> = function
            .params
            .iter()
            .zip(function.ty.params.iter())
            .map(|(local, param_type)| (*local, param_type.clone(), ArgumentPurpose::Normal))
            .collect();

        // Add implicit @return_addr parameter for struct return values.
        if let return_type @ Type::Struct(_) = &*function.ty.return_type {
            fn_params.push((
                LocalId(usize::MAX),
                return_type.clone(),
                ArgumentPurpose::StructReturn,
            )); // TODO: usize::MAX is not very clean
        }

        let returns = match &*function.ty.return_type {
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
                .map(|(_, ty, purpose)| {
                    let mut abi_param: AbiParam =
                        self.get_abi_param(ty).expect("Param type must be valid.");
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

        let crane_blocks: Vec<Block> = function
            .blocks
            .iter()
            .map(|_| fn_builder.create_block())
            .collect();
        let entry_block = crane_blocks[function.entry.0];
        fn_builder.append_block_params_for_function_params(entry_block);
        fn_builder.switch_to_block(entry_block);

        let mut fn_generator = FnGenerator {
            state: self,
            builder: fn_builder,
            var_count: 0,
            current_func_type: function.ty.clone(),
            return_dest: None,
            local_storage: HashMap::new(),
            crane_blocks,
        };

        for (idx, local) in function.locals.iter().enumerate() {
            fn_generator.new_stack_slot_for_local(LocalId(idx), &local.ty);
        }

        for (i, (local_id, param_type, purpose)) in fn_params.iter().enumerate() {
            let param_value = fn_generator.builder.block_params(entry_block)[i];

            let storage_type = if *purpose == ArgumentPurpose::StructReturn {
                Type::Ref(Box::new(param_type.clone()))
            } else {
                param_type.clone()
            };

            let slot = if *purpose == ArgumentPurpose::StructReturn {
                let slot = fn_generator.new_stack_slot_for_local(*local_id, &storage_type);
                debug_assert!(fn_generator.return_dest.is_none());
                fn_generator.return_dest = Some(slot);
                slot
            } else {
                *fn_generator
                    .local_storage
                    .get(local_id)
                    .expect("param local must have stack slot")
            };

            fn_generator.store_to_slot(slot, &storage_type, param_value);
        }

        fn_generator.build_blocks(&function);

        fn_generator.builder.finalize();
        self.module
            .define_function(self.crane_functions[&lower_func_id], &mut fn_context)
            .map_err(|err| GeneratorErrors::ModuleErrors(vec![err]))?;
        self.module.clear_context(&mut fn_context);

        Ok(())
    }
    /*
    fn build_assoc_fn_body(
        &mut self,
        owner: TypeId,
        decl: &FunctionDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        let LowerSymbol::Function(func_id, func_type) = self
            .types
            .associated_functions
            .get(&owner)
            .and_then(|map| map.get(&decl.name))
            .cloned()
            .expect("Associated function must be pre-declared before building its body")
        else {
            unreachable!("associated function table must only contain functions");
        };

        self.build_fn_body_with_symbol(decl, unreachable!("func_id"), func_type, span, false)
    }*/

    // Used for nested function declarations inside function bodies (single-pass is fine there).
    /*fn build_fn_decl(&mut self, decl: &FunctionDecl, span: &SimpleSpan) -> Result<(), ManyErrors> {
        self.pre_declare_fn(decl, span, false)?;
        self.build_fn_body(decl, span, false)
    }*/

    /*fn type_from_path(&self, path: &[Spanned<String>]) -> Option<Type> {
        if path.len() == 1 {
            return self.type_from_name(&path[0].0, false);
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
    }*/
}

impl FnGenerator<'_, '_> {
    pub fn build_blocks(&mut self, function: &MirFunction) {
        for block in &function.blocks {
            let crane_block = self.crane_blocks[block.id.0];
            if self.builder.current_block() != Some(crane_block) {
                self.builder.switch_to_block(crane_block);
            }
            self.build_block(&block);
        }
        self.builder.seal_all_blocks();
    }

    pub fn build_block(&mut self, block: &BasicBlock) {
        let BasicBlock {
            statements,
            terminator,
            ..
        } = block;

        for statement in statements {
            self.build_statement(statement);
        }

        match terminator {
            MirTerminator::Return(Some(operand)) => {
                let (value, ty) = self.build_operand(operand);

                match &ty {
                    Type::Void => {
                        unreachable!(
                            "void must be handled above. Ok im actually unsure about that. We'll see."
                        )
                    }
                    Type::Struct(struct_id) => {
                        let return_dest = self
                            .return_dest
                            .expect("Return destination must be valid when returning struct.");

                        let return_dest = self.stack_slot_addr(return_dest);
                        let return_dest = self.builder.ins().load(
                            self.state.isa.pointer_type(),
                            MemFlags::trusted(),
                            return_dest,
                            0,
                        );
                        self.copy_struct_value(*struct_id, value, return_dest);
                        self.builder.ins().return_(&[]);
                    }
                    _ => {
                        self.builder.ins().return_(&[value]);
                    }
                }
            }
            MirTerminator::Return(None) => {
                self.builder.ins().return_(&[]);
            }
            MirTerminator::Jump(block_id) => {
                self.builder.ins().jump(self.crane_blocks[block_id.0], []);
            }
            MirTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                let (cond, ty) = self.build_scalar_operand(cond);
                debug_assert_eq!(ty, Type::Bool);
                self.builder.ins().brif(
                    cond,
                    self.crane_blocks[then_block.0],
                    [],
                    self.crane_blocks[else_block.0],
                    [],
                );
            }
            MirTerminator::Unreachable => {
                self.builder.ins().trap(UNREACHABLE);
            }
        }
    }

    /* fn build_call_arg(
        &mut self,
        callee_name: &str,
        arg: &Spanned<Arg>,
        param_type: &Type,
    ) -> Result<(Value, Type), ManyErrors> {
        if let Type::Ref(inner) = param_type {
            if !arg.0.is_ref {
                return CodeError::ExpectedRefArgument.err(arg.0.expr.1);
            }

            let place = self.build_place(&arg.0.expr)?;

            if !can_coerce_type(&place.ty, inner.as_ref()) {
                return CodeError::FunctionCallArgumentType(
                    callee_name.to_owned(),
                    inner.to_string(),
                    place.ty.to_string(),
                )
                .err(arg.0.expr.1);
            }
            Ok((place.addr, param_type.clone()))
        } else {
            if arg.0.is_ref {
                return CodeError::UnExpectedRefArgument.err(arg.0.expr.1);
            }
            let value = self.build_expression(&arg.0.expr, Some(param_type.clone()))?;

            let ty = match &value {
                ExprValue::Never => {
                    return Ok((Value::from_u32(u32::MAX - 1), Type::Never));
                }
                ExprValue::Error => {
                    return Ok((Value::from_u32(u32::MAX - 1), Type::Error));
                }
                ExprValue::Void => &Type::Void,
                ExprValue::Ssa(_, ty) | ExprValue::Slot(_, ty) | ExprValue::Addr(_, ty) => ty,
            };

            if !can_coerce_type(ty, param_type) {
                return CodeError::FunctionCallArgumentType(
                    callee_name.to_owned(),
                    param_type.to_string(),
                    ty.to_string(),
                )
                .err(arg.0.expr.1);
            }

            Ok(self
                .get_value(value)
                .expect("ssa or stack slot function argument must produce a value"))
        }
    }*/

    fn build_fn_call(&mut self, callee: &MirCallee, args: &[Operand]) -> Option<GenValue> {
        let func = self.state.function_types[&callee.0].clone();

        let mut arg_values: Vec<Value> = args
            .iter()
            .zip(func.params.iter())
            .map(|(arg, param_ty)| self.build_call_arg(arg, param_ty))
            .collect();

        let return_slot = if let Type::Struct(struct_id) = &*func.return_type {
            let (size, align_shift) = self.state.types.layout(&Type::Struct(*struct_id));
            if func.is_extern {
                todo!("Extern return structs.");
            }
            let stack_slot_data =
                StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift);
            let stack_slot = self.builder.create_sized_stack_slot(stack_slot_data);

            let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
            arg_values.push(struct_addr);

            Some(stack_slot)
        } else {
            None
        };

        let func_ref = self
            .state
            .module
            .declare_func_in_func(self.state.crane_functions[&callee.0], self.builder.func);
        let inst = self.builder.ins().call(func_ref, &arg_values);
        match self.builder.inst_results(inst) {
            // Struct return
            [] if *func.return_type != Type::Void => {
                let addr =
                    self.stack_slot_addr(return_slot.expect("struct return slot must be defined"));
                Some(GenValue::Addr(addr, (*func.return_type).clone()))
            }
            [] => None,
            [value] => Some(GenValue::Ssa(*value, (*func.return_type).clone())),
            _ => panic!("Invalid number of return values. Must be 0 or 1."),
        }
    }

    /*fn build_fn_call_old(
        &mut self,
        callee: &Spanned<Expression>,
        args: &[Spanned<Arg>],
        span: &SimpleSpan,
    ) -> Result<ExprValue, ManyErrors> {
        let callee_name = expr_callee_name(&callee.0);

        let (func_id, func_type, receiver) = match self.resolve_expr_as_symbol(callee) {
            Some(Symbol::Function(id, ft)) => (*id, ft.clone(), None),
            _ => match &callee.0 {
                Expression::MemberAccess(receiver, method_name) => {
                    let receiver_place = self.build_place(receiver)?;
                    let Some((id, ft)) = self.resolve_method_call(receiver, &method_name.0) else {
                        return CodeError::UnknownFunction(callee_name).err(*span);
                    };
                    (id, ft, Some(receiver_place))
                }
                _ => return CodeError::UnknownFunction(callee_name).err(*span),
            },
        };

        let explicit_param_count = if receiver.is_some() {
            func_type.params.len().checked_sub(1).ok_or_else(|| {
                CodeError::FunctionCallArgumentCount(callee_name.clone(), 1, 0).spanned(*span)
            })?
        } else {
            func_type.params.len()
        };

        if args.len() != explicit_param_count {
            return CodeError::FunctionCallArgumentCount(
                callee_name.clone(),
                explicit_param_count,
                args.len(),
            )
            .err(*span);
        }

        let mut arg_values = Vec::new();
        let param_offset = if let Some(receiver) = receiver {
            let param_type = &func_type.params[0];
            if let Type::Ref(inner) = param_type {
                if !can_coerce_type(&receiver.ty, inner.as_ref()) {
                    return CodeError::FunctionCallArgumentType(
                        callee_name.clone(),
                        inner.to_string(),
                        receiver.ty.to_string(),
                    )
                    .err(callee.1);
                }

                arg_values.push((receiver.addr, param_type.clone()));
            } else {
                if !can_coerce_type(&receiver.ty, param_type) {
                    return CodeError::FunctionCallArgumentType(
                        callee_name.clone(),
                        param_type.to_string(),
                        receiver.ty.to_string(),
                    )
                    .err(callee.1);
                }

                arg_values.push(
                    self.get_value(ExprValue::Addr(receiver.addr, receiver.ty))
                        .expect("method receiver must produce a value"),
                );
            }
            1
        } else {
            0
        };

        arg_values.extend(
            args.iter()
                .zip(func_type.params[param_offset..].iter())
                .map(|(arg, param_type)| self.build_call_arg(&callee_name, arg, param_type))
                .collect::<Result<Vec<_>, ManyErrors>>()?,
        );

        if arg_values.iter().any(|(_, ty)| *ty == Type::Never) {
            return Ok(ExprValue::Never);
        }

        if arg_values.iter().any(|(_, ty)| *ty == Type::Error) {
            return Ok(ExprValue::Error);
        }

        let mut arg_values = arg_values
            .into_iter()
            .map(|(value, _)| value)
            .collect::<Vec<_>>();

        let return_slot = if let Type::Struct(struct_id) = &*func_type.return_type {
            let (size, align_shift) = self.state.types.layout(&Type::Struct(*struct_id));
            if func_type.is_extern {
                todo!("Extern return structs.");
            }
            let stack_slot_data =
                StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift);
            let stack_slot = self.builder.create_sized_stack_slot(stack_slot_data);

            let struct_addr = self.builder.ins().stack_addr(types::I64, stack_slot, 0);
            arg_values.push(struct_addr);

            Some(stack_slot)
        } else {
            None
        };

        let func_ref = self
            .state
            .module
            .declare_func_in_func(func_id, self.builder.func);
        let inst = self.builder.ins().call(func_ref, &arg_values);
        match self.builder.inst_results(inst) {
            // Struct return
            [] if *func_type.return_type != Type::Void => Ok(ExprValue::Slot(
                return_slot.expect("struct return slot must be defined"),
                *func_type.return_type,
            )),
            [] => Ok(ExprValue::Void),
            [value] => Ok(ExprValue::Ssa(*value, *func_type.return_type)),
            _ => panic!("Invalid number of return values. Must be 0 or 1."),
        }
    }*/
}

impl GeneratorState<'_> {
    /*
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
                    ty: field_type,
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

        Ok(struct_id)
    }

    fn build_struct_decl(&mut self, decl: &StructDecl) -> Result<(), ManyErrors> {
        let struct_id = self.declare_struct(decl)?;

        let mut errors = ManyErrors::new();
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
            .insert_symbol(decl.name.0.clone(), Symbol::Type(Type::Enum(enum_id)))
            .expect("Must not overwrite symbol");

        Ok(enum_id)
    }

    fn build_enum_decl(&mut self, decl: &EnumDecl) -> Result<(), ManyErrors> {
        self.declare_enum(decl)?;
        Ok(())
    }*/

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
            // FIXME: Structs are memory values; audit remaining callers that treat them as scalar pointer types.
            Type::Struct(_) => self.isa.pointer_type(),
            Type::Enum(_) => types::I32,
            Type::Ref(_) => self.isa.pointer_type(),
            _ => return None,
        })
    }
}

/* fn expr_callee_name(expr: &Expression) -> String {
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
} */

#[cfg(test)]
mod tests {
    /*fn generation_errors(src: &str) -> ManyErrors {
        let path = Path::new("test.luga");
        let program = parse_source(src, false)
            .unwrap_or_else(|err| panic!("test source must parse: {}", err.short_reason()));
        todo!();
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
	var bad = "x" + 1;
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
	var bad = "x" + 1;
	if bad then {
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

    #[test]
    fn poisoned_binding_can_be_used_without_panicking() {
        let errors = generation_errors(
            r#"
fn needs_ref(ref value: i32) {
}

fn main() {
	var value = 1;
	var poisoned = needs_ref(value);
	needs_ref(ref poisoned);
}
"#,
        );

        assert!(
            errors
                .iter()
                .any(|Spanned(error, _)| matches!(error, CodeError::ExpectedRefArgument))
        );
    }*/
}
