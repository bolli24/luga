use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use cranelift::codegen::Context;
use cranelift::codegen::ir::{Function, UserExternalName, UserFuncName};
use cranelift::module::DataId;
use cranelift::prelude::isa::OwnedTargetIsa;
use cranelift::{
    codegen::ir::{ArgumentPurpose, StackSlot},
    module::{DataDescription, FuncId, Linkage, Module, default_libcall_names},
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{isa::CallConv, *},
};
use indexmap::IndexMap;
use luga_syntax::parser::{BinOp, Intrinsic, UnOp};

use crate::errors::*;
use luga_check::lower::ModuleId;
use luga_check::lower_state::LowerFuncId;
use luga_check::mir::{
    BasicBlock, FieldId, LocalId, MirCallee, MirConst, MirFunction, MirProgram, MirStatement,
    MirTerminator, Operand, Place as MirPlace, Rvalue,
};
use luga_check::types::{Type, *};

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

/// Text of the generated code, e.g. for `luga build --emit`. Set a field to `Some` to request it;
/// `generate` appends each function to it.
#[derive(Debug, Default)]
pub struct Listings {
    /// Cranelift IR of each function, as built from the MIR, before Cranelift optimizes it
    pub clif: Option<String>,
    /// Machine code of each function, as Cranelift prints it
    pub asm: Option<String>,
}

impl Listings {
    fn append(text: &mut String, function: &str, body: &str) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("; fn {function}\n{}\n", body.trim_end()));
    }
}

pub fn generate(
    program: MirProgram,
    file_path: Option<&Path>,
    release: bool,
    verbose: bool,
    listings: &mut Listings,
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
        listings,
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
    isa: OwnedTargetIsa,
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
    listings: &'a mut Listings,
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
                    todo!("Extern strcut arguments");
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
            Type::Ref(_) => self.isa.pointer_type(),
            Type::Void | Type::Error | Type::Never | Type::Fn(_) => return None,
        })
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

        match ty {
            Type::Bool if *op == UnOp::Not => {
                let result = self.builder.ins().bxor_imm(value, 1);
                GenValue::Ssa(result, Type::Bool)
            }
            Type::I8 | Type::I16 | Type::I32 | Type::I64 if *op == UnOp::Neg => {
                let result = self.builder.ins().ineg(value);
                GenValue::Ssa(result, ty)
            }
            Type::F64 | Type::F32 if *op == UnOp::Neg => {
                let result = self.builder.ins().fneg(value);
                GenValue::Ssa(result, ty)
            }
            _ => unreachable!(),
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

    fn align_to(offset: i32, align_shift: u8) -> i32 {
        let align = 1 << align_shift;
        (offset + align - 1) & !(align - 1)
    }

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
        // Only used in listings, which would otherwise show every function as the default u0:0
        fn_context.func.name = UserFuncName::user(0, self.crane_functions[&lower_func_id].as_u32());
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

        if self.listings.clif.is_some() {
            let names = self.reference_names(&fn_context.func);
            let text = rename_tokens(&fn_context.func.display().to_string(), &names);
            if let Some(clif) = &mut self.listings.clif {
                Listings::append(clif, &function.name, &text);
            }
        }
        fn_context.set_disasm(self.listings.asm.is_some());
        self.module
            .define_function(self.crane_functions[&lower_func_id], &mut fn_context)
            .map_err(|err| GeneratorErrors::CraneModuleErrors(vec![err]))?;
        if let Some(vcode) = fn_context
            .compiled_code()
            .and_then(|code| code.vcode.as_deref())
            && self.listings.asm.is_some()
        {
            let text = rename_tokens(vcode, &self.reference_names(&fn_context.func));
            if let Some(asm) = &mut self.listings.asm {
                Listings::append(asm, &function.name, &text);
            }
        }
        self.module.clear_context(&mut fn_context);

        Ok(())
    }

    /// Declared names for the functions and data `func` refers to, keyed by the tokens Cranelift
    /// prints for them: `u0:2` (namespace 0 holds functions, 1 holds data) and the function's own
    /// references to those, `userextname1`. Names get CLIF's `%` sigil, e.g. `%fibonacci`.
    fn reference_names(&self, func: &Function) -> HashMap<String, String> {
        let declarations = self.module.declarations();
        let declared_name = |user: &UserExternalName| match user.namespace {
            0 => declarations
                .get_function_decl(FuncId::from_u32(user.index))
                .name
                .clone(),
            1 => declarations
                .get_data_decl(DataId::from_u32(user.index))
                .name
                .clone(),
            _ => None,
        };

        let mut names = HashMap::new();
        let mut add = |user: &UserExternalName, reference: Option<String>| {
            if let Some(name) = declared_name(user) {
                let name = format!("%{name}");
                if let Some(reference) = reference {
                    names.insert(reference, name.clone());
                }
                names.insert(format!("u{}:{}", user.namespace, user.index), name);
            }
        };
        if let UserFuncName::User(user) = &func.name {
            add(user, None);
        }
        for (reference, user) in func.params.user_named_funcs() {
            add(user, Some(reference.to_string()));
        }
        names
    }
}

/// Replace each token in `text` that `names` has an entry for. Tokens are runs of letters, digits,
/// `_` and `:`, so `u0:2` is one token and doesn't match inside `u0:21`.
fn rename_tokens(text: &str, names: &HashMap<String, String>) -> String {
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':';
    let mut renamed = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_token_char) {
        renamed.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c| !is_token_char(c)).unwrap_or(rest.len());
        let token = &rest[..end];
        renamed.push_str(names.get(token).map_or(token, String::as_str));
        rest = &rest[end..];
    }
    renamed.push_str(rest);
    renamed
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

    fn build_fn_call(&mut self, callee: &MirCallee, args: &[Operand]) -> Option<GenValue> {
        let func = self.state.function_types[&callee.0].clone();

        let mut arg_values: Vec<Value> = args
            .iter()
            .zip(func.params.iter())
            .map(|(arg, param_ty)| {
                if !(param_ty.is_int()
                    || *param_ty == Type::String
                    || *param_ty == Type::Bool
                    || param_ty.is_enum()
                    || (param_ty.is_struct()
                        && self.state.isa.default_call_conv() == CallConv::WindowsFastcall))
                    && func.is_extern
                {
                    todo!("Unsupported extern argument type: {param_ty}");
                }
                self.build_call_arg(arg, param_ty)
            })
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
}

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

    use super::*;

    #[test]
    fn renames_whole_tokens_only() {
        let names = HashMap::from([
            ("u0:2".to_string(), "%fib".to_string()),
            ("userextname1".to_string(), "%string_0".to_string()),
        ]);
        assert_eq!(
            rename_tokens(
                "fn0 = colocated u0:2 sig0, u0:21\n  call User(userextname1); userextname12",
                &names
            ),
            "fn0 = colocated %fib sig0, u0:21\n  call User(%string_0); userextname12"
        );
    }
}
