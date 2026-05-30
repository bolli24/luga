use luga_syntax::{
    BinOp, Expression, FunctionDecl, If, Intrinsic, SimpleSpan, Span, Spanned, Statement,
    StructDecl, StructInit, UnOp, parser::Arg,
};
use std::collections::HashMap;

use crate::{
    CodeError, ManyErrors,
    mir::{
        BasicBlock, BlockId, LocalId, LocalKind, MirCallee, MirConst, MirFunction, MirLocal,
        MirStatement, MirTerminator, Operand, Place, Rvalue,
    },
    types::{FuncType, Type, TypeId},
};

use crate::lower_scope::{LookupResult, LowerScope, LowerSymbol};
use crate::lower_state::{LowerFuncId, LowerState};
use crate::mir::FieldId;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub usize);

pub struct MirLowerer<'a> {
    state: &'a mut LowerState,
    locals: Vec<MirLocal>,
    local_scope: LocalScope,
    blocks: Vec<BasicBlock>,
    current_block: BlockId,
    current_func_type: FuncType,
    result_span: Option<SimpleSpan>,
    nested_functions: HashMap<LowerFuncId, MirFunction>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoweredValue {
    Value { operand: Operand },
    Void,
    Never,
    Error,
}

impl LoweredValue {
    pub fn ty(&self) -> &Type {
        match self {
            LoweredValue::Value { operand } => operand.ty(),
            LoweredValue::Void => &Type::Void,
            LoweredValue::Never => &Type::Never,
            LoweredValue::Error => &Type::Error,
        }
    }

    pub fn operand(self) -> Option<Operand> {
        match self {
            LoweredValue::Value { operand } => Some(operand),
            LoweredValue::Void | LoweredValue::Never | LoweredValue::Error => None,
        }
    }

    pub fn usable_operand(self) -> Option<(Operand, Type)> {
        match self {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                Some((operand, ty))
            }
            LoweredValue::Void | LoweredValue::Never | LoweredValue::Error => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continues,
    Terminates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchResult {
    Value,
    Void,
    NoContinue,
}

pub fn can_coerce_type(actual: &Type, expected: &Type) -> bool {
    matches!(actual, Type::Never | Type::Error)
        || actual == expected
        || matches!(actual, Type::Ref(inner) if inner.as_ref() == expected)
}

impl<'state> MirLowerer<'state> {
    pub fn new(state: &'state mut LowerState, current_func_type: FuncType) -> Self {
        let entry = BlockId(0);

        Self {
            state,
            locals: Vec::new(),
            local_scope: LocalScope::new(),
            blocks: vec![BasicBlock {
                id: entry,
                label: Some("entry".to_string()),
                statements: Vec::new(),
                terminator: MirTerminator::Unreachable,
            }],
            current_block: entry,
            current_func_type,
            result_span: None,
            nested_functions: HashMap::new(),
        }
    }

    pub fn new_local(&mut self, local: MirLocal) -> LocalId {
        let id = LocalId(self.locals.len());
        self.locals.push(local);
        id
    }

    pub fn new_error_local(&mut self, name: String) -> LocalId {
        self.new_local(MirLocal {
            name: Some(name),
            ty: Type::Error,
            kind: LocalKind::User,
        })
    }

    pub fn define_local(&mut self, name: String, local: LocalId) -> Option<LocalId> {
        self.local_scope.define(name, local)
    }

    pub fn lookup_local(&self, name: &str) -> Option<LocalId> {
        self.local_scope.lookup(name)
    }

    pub fn push_statement(&mut self, statement: MirStatement) {
        self.current_block_mut().statements.push(statement);
    }

    pub fn set_terminator(&mut self, terminator: MirTerminator) {
        self.current_block_mut().terminator = terminator;
    }

    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(BasicBlock {
            id,
            label: None,
            statements: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        id
    }

    pub fn new_block_labelled(&mut self, label: impl Into<String>) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(BasicBlock {
            id,
            label: Some(label.into()),
            statements: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        id
    }

    pub fn switch_to_block(&mut self, block: BlockId) {
        debug_assert!(block.0 < self.blocks.len());
        self.current_block = block;
    }

    pub fn finish(self, name: String, params: Vec<LocalId>, func_type: FuncType) -> MirFunction {
        let type_names = self
            .state
            .types
            .structs
            .keys()
            .chain(self.state.types.enums.keys())
            .map(|id| (*id, self.state.types.type_name(*id)))
            .collect();

        MirFunction {
            name,
            params,
            locals: self.locals,
            blocks: self.blocks,
            entry: BlockId(0),
            ty: func_type,
            type_names,
        }
    }

    pub fn into_nested_functions(self) -> HashMap<LowerFuncId, MirFunction> {
        self.nested_functions
    }

    fn current_block_mut(&mut self) -> &mut BasicBlock {
        &mut self.blocks[self.current_block.0]
    }

    fn lower_block_body(
        &mut self,
        statements: &[Spanned<Statement>],
        try_into_type: Option<Type>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        self.local_scope.enter_scope();
        self.state.push_scope();

        let result = self.lower_statements(statements, try_into_type);

        self.local_scope.exit_scope();
        self.state
            .pop_scope()
            .expect("block scope must have parent scope");

        result
    }

    fn lower_nested_function(
        &mut self,
        decl: &FunctionDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        let (func_id, func_type) = self
            .state
            .current_scope
            .get_function(&decl.name)
            .map(|(id, ft)| (id, ft.clone()))
            .expect("Function must be pre-declared before lowering its body");

        let outer_scope = std::mem::replace(&mut self.state.current_scope, LowerScope::new());
        self.state.current_scope = outer_scope.function_scope();

        let lowerer = MirLowerer::new(self.state, func_type);
        let result = lowerer.lower_function(func_id, decl, span, false);

        self.state.current_scope = *self
            .state
            .current_scope
            .parent
            .take()
            .expect("mir nested function scope must have parent");

        match result {
            Ok(functions) => {
                self.nested_functions.extend(functions);
                Ok(())
            }
            Err(errors) => Err(errors),
        }
    }

    fn lower_associated_functions(
        &mut self,
        owner: TypeId,
        decl: &StructDecl,
    ) -> Result<(), ManyErrors> {
        let mut errors = ManyErrors::new();

        for func_decl in &decl.functions {
            if let Err(mut new_errors) =
                self.lower_associated_function(owner, &func_decl.0, &func_decl.1)
            {
                errors.append(&mut new_errors);
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn lower_associated_function(
        &mut self,
        owner: TypeId,
        decl: &FunctionDecl,
        span: &SimpleSpan,
    ) -> Result<(), ManyErrors> {
        let LowerSymbol::Function(func_id, func_type) = self
            .state
            .types
            .associated_functions
            .get(&owner)
            .and_then(|map| map.get(&decl.name))
            .cloned()
            .expect("Associated function must be pre-declared before lowering its body")
        else {
            unreachable!("associated function table must only contain functions");
        };

        let outer_scope = std::mem::replace(&mut self.state.current_scope, LowerScope::new());
        self.state.current_scope = outer_scope.function_scope();

        let lowerer = MirLowerer::new(self.state, func_type.clone());
        let result = lowerer.lower_function_with_type(func_id, decl, span, false, func_type);

        self.state.current_scope = *self
            .state
            .current_scope
            .parent
            .take()
            .expect("mir associated function scope must have parent");

        match result {
            Ok(functions) => {
                self.nested_functions.extend(functions);
                Ok(())
            }
            Err(errors) => Err(errors),
        }
    }

    pub fn lower_function(
        self,
        func_id: LowerFuncId,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        is_top_level: bool,
    ) -> Result<HashMap<LowerFuncId, MirFunction>, ManyErrors> {
        let (_, func_type) = self
            .state
            .current_scope
            .get_function(&decl.name)
            .map(|(id, ft)| (id, ft.clone()))
            .expect("Function must be pre-declared before lowering its body");

        self.lower_function_with_type(func_id, decl, span, is_top_level, func_type)
    }

    pub fn lower_function_with_type(
        mut self,
        func_id: LowerFuncId,
        decl: &FunctionDecl,
        span: &SimpleSpan,
        is_top_level: bool,
        func_type: FuncType,
    ) -> Result<HashMap<LowerFuncId, MirFunction>, ManyErrors> {
        let mut errors = ManyErrors::new();

        let params = decl
            .params
            .iter()
            .zip(func_type.params.iter())
            .map(|(param, param_type)| {
                let local = self.new_local(MirLocal {
                    name: Some(param.name.0.clone()),
                    ty: param_type.clone(),
                    kind: LocalKind::Param,
                });
                self.define_local(param.name.0.clone(), local);
                self.state
                    .current_scope
                    .insert_symbol(param.name.0.clone(), LowerSymbol::Local(param_type.clone()))
                    .expect("function parameter must not already exist in scope");
                local
            })
            .collect::<Vec<_>>();

        let return_type = *func_type.return_type.clone();

        let main_implicit_zero =
            is_top_level && !decl.r#extern && decl.name == "main" && decl.return_type.is_none();

        match self.lower_statements(&decl.body, Some(return_type.clone())) {
            Err(mut new_errors) => errors.append(&mut new_errors),
            Ok((_, Control::Terminates)) => {}
            Ok((LoweredValue::Never | LoweredValue::Error, _)) => {}
            Ok((LoweredValue::Void, _)) if return_type == Type::Void => {
                self.set_terminator(MirTerminator::Return(None));
            }
            Ok((LoweredValue::Void, _)) if main_implicit_zero => {
                self.set_terminator(MirTerminator::Return(Some(Operand::Const(
                    MirConst::Integer(0),
                    Type::I32,
                ))));
            }
            Ok((LoweredValue::Void, _)) => {
                errors.push(CodeError::MissingReturnStatement.spanned(*span));
            }
            Ok((LoweredValue::Value { operand }, _)) => {
                let ty = operand.ty().clone();
                if !can_coerce_type(&ty, &return_type) {
                    errors.push(
                        CodeError::InvalidReturnType(return_type.to_string(), ty.to_string())
                            .spanned(self.result_span.unwrap_or(*span)),
                    );
                } else {
                    self.set_terminator(MirTerminator::Return(Some(operand)));
                }
            }
        }

        if errors.is_empty() {
            let nested_functions = std::mem::take(&mut self.nested_functions);
            let mut functions =
                HashMap::from([(func_id, self.finish(decl.name.clone(), params, func_type))]);
            functions.extend(nested_functions);
            Ok(functions)
        } else {
            Err(errors)
        }
    }

    fn lower_statements(
        &mut self,
        statements: &[Spanned<Statement>],
        try_into_type: Option<Type>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let mut errors = ManyErrors::new();
        let mut control = Control::Continues;
        let mut value = LoweredValue::Void;
        let mut iter = statements.iter().peekable();

        while let Some(statement) = iter.next() {
            if matches!(control, Control::Terminates) {
                errors.push(CodeError::UnreachableStatement.spanned(statement.1));
                continue;
            }

            match &statement.0 {
                Statement::Result(expr) => {
                    self.result_span = Some(expr.1);
                    match self.lower_expr(expr, try_into_type.clone()) {
                        Ok(result) => {
                            let (result_value, result_control) = result;
                            control = result_control;
                            value = result_value;
                            if iter.peek().is_some() {
                                errors.push(CodeError::ResultStatementNotLast.spanned(statement.1));
                                break;
                            }
                        }
                        Err(mut new_errors) => errors.append(&mut new_errors),
                    }
                }

                _ => match self.lower_statement(statement) {
                    Ok((_, next_control)) => control = next_control,
                    Err(mut new_errors) => errors.append(&mut new_errors),
                },
            }
        }

        if errors.is_empty() {
            Ok((value, control))
        } else {
            Err(errors)
        }
    }
    fn lower_statement(
        &mut self,
        statement: &Spanned<Statement>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        match &statement.0 {
            Statement::Expression(expr) => {
                let value = self.lower_expr(expr, None)?;
                if value.0 == LoweredValue::Never {
                    return Ok(value);
                }
            }
            Statement::Binding(name, expr) => {
                if self.local_scope.contains_in_current_scope(name)
                    || self
                        .state
                        .current_scope
                        .contains_symbol_in_current_scope(name)
                {
                    return CodeError::VariableRedefinition(name.clone()).err(statement.1);
                }

                let value = match self.lower_expr(expr, None) {
                    Ok(value) => value,
                    Err(errors) => {
                        let local = self.new_error_local(name.to_owned());
                        self.define_local(name.clone(), local);
                        self.state
                            .current_scope
                            .insert_symbol(name.clone(), LowerSymbol::Local(Type::Error))
                            .expect("error local must not already exist in scope");
                        return Err(errors);
                    }
                };
                let operand = match value.0 {
                    LoweredValue::Value { operand } => operand,
                    LoweredValue::Void => {
                        return CodeError::BindingType(Type::Void.to_string()).err(expr.1);
                    }
                    LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                    LoweredValue::Error => {
                        let local = self.new_error_local(name.clone());
                        self.define_local(name.clone(), local);
                        self.state
                            .current_scope
                            .insert_symbol(name.clone(), LowerSymbol::Local(Type::Error))
                            .expect("error local must not already exist in scope");
                        return Ok((
                            LoweredValue::Value {
                                operand: Operand::Place(Place::Local(local, Type::Error)),
                            },
                            Control::Continues,
                        ));
                    }
                };

                let ty = operand.ty();
                let local = self.new_local(MirLocal {
                    name: Some(name.clone()),
                    kind: LocalKind::User,
                    ty: ty.clone(),
                });
                self.define_local(name.clone(), local);
                self.state
                    .current_scope
                    .insert_symbol(name.clone(), LowerSymbol::Local(ty.clone()))
                    .expect("local binding must not already exist in scope");
                self.push_statement(MirStatement::Assign(
                    Place::Local(local, ty.clone()),
                    Rvalue::Use(operand),
                ));
            }
            Statement::Assignment(target, expr) => {
                let place = self.lower_place(target)?;
                let place_type = place.ty();
                let value = self.lower_expr(expr, Some(place_type.clone()))?;

                let rvalue = match value.0 {
                    LoweredValue::Value { operand } => {
                        let ty = operand.ty().clone();
                        if !can_coerce_type(&ty, place_type) {
                            return CodeError::AssignmentIncompatibleTypes(
                                place.ty().to_string(),
                                ty.to_string(),
                            )
                            .err(statement.1);
                        }

                        Rvalue::Use(operand)
                    }
                    LoweredValue::Void => {
                        return CodeError::AssignmentIncompatibleTypes(
                            place.ty().to_string(),
                            Type::Void.to_string(),
                        )
                        .err(statement.1);
                    }
                    LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                    LoweredValue::Error => return Ok((LoweredValue::Void, Control::Continues)),
                };

                self.push_statement(MirStatement::Assign(place, rvalue));
            }
            Statement::Return(expr) => {
                let value =
                    self.lower_expr(expr, Some(*self.current_func_type.return_type.clone()))?;

                let ty = value.0.ty();

                if !can_coerce_type(&ty, &self.current_func_type.return_type) {
                    return CodeError::InvalidReturnType(
                        self.current_func_type.return_type.to_string(),
                        ty.to_string(),
                    )
                    .err(expr.1);
                }

                return Ok(match value.0 {
                    LoweredValue::Value { operand, .. } => {
                        self.set_terminator(MirTerminator::Return(Some(operand)));
                        (LoweredValue::Never, Control::Terminates)
                    }
                    LoweredValue::Void => {
                        self.set_terminator(MirTerminator::Return(None));
                        (LoweredValue::Never, Control::Terminates)
                    }
                    LoweredValue::Never => (LoweredValue::Never, Control::Terminates),
                    LoweredValue::Error => (LoweredValue::Void, Control::Continues),
                });
            }
            Statement::While(conditional) => return self.lower_while(conditional),
            Statement::FunctionDecl(function_decl) => {
                self.state
                    .pre_declare_fn(function_decl, &statement.1, false)?;
                self.lower_nested_function(function_decl, &statement.1)?;
                return Ok((LoweredValue::Void, Control::Continues));
            }
            Statement::StructDecl(struct_decl) => {
                let struct_id = self.state.declare_struct(struct_decl)?;
                self.lower_associated_functions(struct_id, struct_decl)?;
                return Ok((LoweredValue::Void, Control::Continues));
            }
            Statement::EnumDecl(enum_decl) => {
                self.state.declare_enum(enum_decl)?;
                return Ok((LoweredValue::Void, Control::Continues));
            }
            Statement::Result(_) => {
                unreachable!("result statements are handled by lower_statements")
            }
        }

        Ok((LoweredValue::Void, Control::Continues))
    }

    fn lower_expr(
        &mut self,
        expr: &Spanned<Expression>,
        try_into_type: Option<Type>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        match &expr.0 {
            Expression::Identifier(_) => Ok((self.lower_place_value(expr)?, Control::Continues)),
            Expression::Float(value) => Ok((
                LoweredValue::Value {
                    operand: Operand::Const(MirConst::Float(*value as f32), Type::F32),
                },
                Control::Continues,
            )),
            Expression::Integer(value) => {
                let into_type = match try_into_type.as_ref() {
                    Some(Type::Error) => {
                        return Ok((LoweredValue::Error, Control::Continues));
                    }
                    Some(ty) if ty.is_int() => ty.clone(),
                    _ => Type::I32,
                };

                let fits = match into_type {
                    Type::I8 => i8::try_from(*value).is_ok(),
                    Type::U8 => u8::try_from(*value).is_ok(),
                    Type::I16 => i16::try_from(*value).is_ok(),
                    Type::U16 => u16::try_from(*value).is_ok(),
                    Type::I32 => i32::try_from(*value).is_ok(),
                    Type::U32 => u32::try_from(*value).is_ok(),
                    Type::I64 => true,
                    Type::U64 => *value >= 0,
                    _ => unreachable!("integer literal target type must be an integer"),
                };

                if !fits {
                    return CodeError::CoerseInt(into_type.to_string(), *value).err(expr.1);
                }

                Ok((
                    LoweredValue::Value {
                        operand: Operand::Const(MirConst::Integer(*value), into_type.clone()),
                    },
                    Control::Continues,
                ))
            }
            Expression::String(value) => Ok((
                LoweredValue::Value {
                    operand: Operand::Const(MirConst::String(value.clone()), Type::String),
                },
                Control::Continues,
            )),
            Expression::Bool(value) => Ok((
                LoweredValue::Value {
                    operand: Operand::Const(MirConst::Bool(*value), Type::Bool),
                },
                Control::Continues,
            )),
            Expression::Block(statements) => self.lower_block_body(statements, try_into_type),
            Expression::If(if_expr) => self.lower_if(if_expr, expr.1, try_into_type),
            Expression::Binary(op, left, right) => {
                let (left_value, left_control) = self.lower_expr(left, try_into_type.clone())?;
                if matches!(left_control, Control::Terminates) {
                    return Ok((LoweredValue::Never, Control::Terminates));
                }

                let (left_operand, left_type) = match left_value {
                    LoweredValue::Value { operand } => {
                        let ty = operand.ty().clone();
                        (operand, ty)
                    }
                    LoweredValue::Void => {
                        return CodeError::InvalidBinOpType(*op, Type::Void.to_string())
                            .err(expr.1);
                    }
                    LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                    LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
                };

                let (right_value, right_control) = self.lower_expr(right, try_into_type)?;
                if matches!(right_control, Control::Terminates) {
                    return Ok((LoweredValue::Never, Control::Terminates));
                }

                let (right_operand, right_type) = match right_value {
                    LoweredValue::Value { operand } => {
                        let ty = operand.ty().clone();
                        (operand, ty)
                    }
                    LoweredValue::Void => {
                        return CodeError::InvalidBinOpType(*op, Type::Void.to_string())
                            .err(expr.1);
                    }
                    LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                    LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
                };

                if left_type != right_type {
                    return CodeError::IncompatipleBinOpTypes(
                        *op,
                        left_type.to_string(),
                        right_type.to_string(),
                    )
                    .err(expr.1);
                }

                let result_type = match &left_type {
                    Type::I8 | Type::I32 | Type::I64 => match op {
                        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => left_type.clone(),
                        BinOp::Equal
                        | BinOp::NotEqual
                        | BinOp::Lt
                        | BinOp::Gt
                        | BinOp::Lte
                        | BinOp::Gte => Type::Bool,
                    },
                    Type::F32 | Type::F64 => match op {
                        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => left_type.clone(),
                        BinOp::Equal
                        | BinOp::NotEqual
                        | BinOp::Lt
                        | BinOp::Gt
                        | BinOp::Lte
                        | BinOp::Gte => Type::Bool,
                    },
                    Type::Bool if matches!(op, BinOp::Equal | BinOp::NotEqual) => Type::Bool,
                    Type::Enum(_) if matches!(op, BinOp::Equal | BinOp::NotEqual) => Type::Bool,
                    _ => {
                        return CodeError::InvalidBinOpType(*op, left_type.to_string()).err(expr.1);
                    }
                };

                let temp = self.new_local(MirLocal {
                    name: None,
                    ty: result_type.clone(),
                    kind: LocalKind::Temp,
                });
                let temp_place = Place::Local(temp, result_type.clone());

                self.push_statement(MirStatement::Assign(
                    temp_place.clone(),
                    Rvalue::Binary {
                        op: *op,
                        left: left_operand,
                        right: right_operand,
                    },
                ));

                Ok((
                    LoweredValue::Value {
                        operand: Operand::Place(temp_place),
                    },
                    Control::Continues,
                ))
            }
            Expression::Unary(op, operand_expr) => {
                let (value, control) = self.lower_expr(operand_expr, Some(Type::Bool))?;
                if matches!(control, Control::Terminates) {
                    return Ok((LoweredValue::Never, Control::Terminates));
                }

                let operand = match value {
                    LoweredValue::Value { operand } => {
                        let ty = operand.ty().clone();
                        if ty != Type::Bool || *op != UnOp::Not {
                            return CodeError::InvalidUnOpType(*op, ty.to_string()).err(expr.1);
                        }

                        operand
                    }
                    LoweredValue::Void => {
                        return CodeError::InvalidUnOpType(*op, Type::Void.to_string()).err(expr.1);
                    }
                    LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                    LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
                };

                let temp = self.new_local(MirLocal {
                    name: None,
                    ty: Type::Bool,
                    kind: LocalKind::Temp,
                });
                let temp_place = Place::Local(temp, Type::Bool);

                self.push_statement(MirStatement::Assign(
                    temp_place.clone(),
                    Rvalue::Unary {
                        op: *op,
                        operand: operand,
                    },
                ));

                Ok((
                    LoweredValue::Value {
                        operand: Operand::Place(temp_place),
                    },
                    Control::Continues,
                ))
            }
            Expression::Intrinsic(intrinsic, args) => self.lower_intrinsic(intrinsic, args, expr.1),
            Expression::FunctionCall(callee, args) => self.lower_fn_call(callee, args, expr.1),
            Expression::MemberAccess(base, member_name) => {
                if let Some(value) = self.lower_symbol_member_value(expr)? {
                    Ok((value, Control::Continues))
                } else {
                    self.lower_field_value(base, member_name)
                }
            }
            Expression::StructInit(struct_init) => self.lower_struct_init(struct_init, expr.1),
        }
    }

    fn lower_place(&self, target: &Spanned<Expression>) -> Result<Place, ManyErrors> {
        match &target.0 {
            Expression::Identifier(name) => {
                if let Some(local) = self.lookup_local(name) {
                    let ty = self.locals[local.0].ty.clone();
                    let place = Place::Local(local, ty.clone());
                    return Ok(match ty {
                        Type::Ref(inner) => Place::Deref {
                            pointer: Box::new(place),
                            ty: *inner,
                        },
                        _ => place,
                    });
                }

                match self.state.current_scope.get_symbol(name) {
                    LookupResult::Found(LowerSymbol::Local(_)) => {
                        CodeError::UnknownVariable(name.clone()).err(target.1)
                    }
                    LookupResult::Found(LowerSymbol::Function(_, func_type)) => {
                        CodeError::FunctionUsedAsValue(func_type.name.clone()).err(target.1)
                    }
                    LookupResult::Found(LowerSymbol::Type(_)) => {
                        CodeError::TypeUsedAsValue(name.clone()).err(target.1)
                    }
                    LookupResult::Found(LowerSymbol::Module(_)) => {
                        CodeError::ModuleUsedAsValue(name.clone()).err(target.1)
                    }
                    LookupResult::CaptureError => CodeError::Capture(name.clone()).err(target.1),
                    LookupResult::NotFound => {
                        CodeError::UnknownVariable(name.clone()).err(target.1)
                    }
                }
            }
            Expression::MemberAccess(base, member_name) => {
                let base_place = self.lower_place(base)?;
                let Type::Struct(struct_id) = base_place.ty() else {
                    return CodeError::UnknownField(
                        base_place.ty().to_string(),
                        member_name.0.clone(),
                    )
                    .err(member_name.1);
                };

                let struct_type = &self.state.types.structs[struct_id];
                let Some(field_index) = struct_type.fields.get_index_of(&member_name.0) else {
                    return CodeError::UnknownField(
                        base_place.ty().to_string(),
                        member_name.0.clone(),
                    )
                    .err(member_name.1);
                };
                let field = struct_type
                    .fields
                    .get(&member_name.0)
                    .expect("field index came from same map");

                Ok(Place::Field {
                    base: Box::new(base_place),
                    field_id: FieldId(field_index),
                    ty: field.ty.clone(),
                })
            }
            _ => CodeError::ExpectedLValue.err(target.1),
        }
    }

    fn lower_place_value(
        &mut self,
        expr: &Spanned<Expression>,
    ) -> Result<LoweredValue, ManyErrors> {
        let place = self.lower_place(expr)?;
        let ty = place.ty().clone();

        if ty == Type::Error {
            return Ok(LoweredValue::Error);
        }

        Ok(LoweredValue::Value {
            operand: Operand::Place(place),
        })
    }

    fn lower_field_value(
        &mut self,
        base: &Spanned<Expression>,
        member_name: &Spanned<String>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let (base_value, control) = self.lower_expr(base, None)?;
        if matches!(control, Control::Terminates) {
            return Ok((LoweredValue::Never, Control::Terminates));
        }

        let (base_operand, base_ty) = match base_value {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                (operand, ty)
            }
            LoweredValue::Void => {
                return CodeError::UnknownField(Type::Void.to_string(), member_name.0.clone())
                    .err(member_name.1);
            }
            LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
            LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
        };

        let Type::Struct(struct_id) = &base_ty else {
            return CodeError::UnknownField(base_ty.to_string(), member_name.0.clone())
                .err(member_name.1);
        };

        let struct_type = &self.state.types.structs[struct_id];
        let Some(field_index) = struct_type.fields.get_index_of(&member_name.0) else {
            return CodeError::UnknownField(base_ty.to_string(), member_name.0.clone())
                .err(member_name.1);
        };
        let field = struct_type
            .fields
            .get(&member_name.0)
            .expect("field index came from same map");
        let field_ty = field.ty.clone();

        let base_place = match base_operand {
            Operand::Place(place) => place,
            operand => {
                let temp = self.new_local(MirLocal {
                    name: None,
                    ty: base_ty.clone(),
                    kind: LocalKind::Temp,
                });
                let temp_place = Place::Local(temp, base_ty.clone());
                self.push_statement(MirStatement::Assign(
                    temp_place.clone(),
                    Rvalue::Use(operand),
                ));
                temp_place
            }
        };

        Ok((
            LoweredValue::Value {
                operand: Operand::Place(Place::Field {
                    base: Box::new(base_place),
                    field_id: FieldId(field_index),
                    ty: field_ty.clone(),
                }),
            },
            Control::Continues,
        ))
    }

    fn lower_symbol_member_value(
        &mut self,
        expr: &Spanned<Expression>,
    ) -> Result<Option<LoweredValue>, ManyErrors> {
        let Expression::MemberAccess(base, member_name) = &expr.0 else {
            return Ok(None);
        };

        let Some(symbol) = self.resolve_expr_as_symbol(base).cloned() else {
            return Ok(None);
        };

        match symbol {
            LowerSymbol::Module(_) => {
                CodeError::ModuleUsedAsValue(member_name.0.clone()).err(member_name.1)
            }
            LowerSymbol::Type(Type::Enum(enum_id)) => {
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

                let ty = Type::Enum(enum_id);
                let temp = self.new_local(MirLocal {
                    name: None,
                    ty: ty.clone(),
                    kind: LocalKind::Temp,
                });
                let temp_place = Place::Local(temp, ty);

                self.push_statement(MirStatement::Assign(
                    temp_place.clone(),
                    Rvalue::EnumVariant {
                        ty: enum_id,
                        value: idx as u32,
                    },
                ));

                Ok(Some(LoweredValue::Value {
                    operand: Operand::Place(temp_place),
                }))
            }
            LowerSymbol::Type(Type::Struct(struct_id)) => {
                CodeError::TypeUsedAsValue(self.state.types.type_name(struct_id)).err(base.1)
            }
            LowerSymbol::Type(_) => {
                unreachable!("Only struct and enum types may be imported.")
            }
            LowerSymbol::Local(..) => {
                unreachable!("resolve_expr_as_symbol must not return variables.")
            }
            LowerSymbol::Function(_, func_type) => {
                CodeError::FunctionUsedAsValue(func_type.name.clone()).err(base.1)
            }
        }
    }

    fn lower_fn_call(
        &mut self,
        callee: &Spanned<Expression>,
        args: &[Spanned<Arg>],
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let callee_name = expr_callee_name(&callee.0);

        let (func_id, func_type, receiver) = match self.resolve_expr_as_symbol(callee).cloned() {
            Some(LowerSymbol::Function(id, ft)) => (id, ft, None),
            _ => match &callee.0 {
                Expression::MemberAccess(receiver, method_name) => {
                    let receiver_place = self.lower_place(receiver)?;
                    let Some((id, ft)) = self.resolve_method_call(receiver, &method_name.0) else {
                        return CodeError::UnknownFunction(callee_name).err(span);
                    };
                    (id, ft, Some(receiver_place))
                }
                _ => return CodeError::UnknownFunction(callee_name).err(span),
            },
        };

        let explicit_param_count = if receiver.is_some() {
            func_type.params.len().checked_sub(1).ok_or_else(|| {
                CodeError::FunctionCallArgumentCount(callee_name.clone(), 1, 0).spanned(span)
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
            .err(span);
        }

        let mut arg_operands = Vec::new();
        let param_offset = if let Some(receiver) = receiver {
            let param_type = &func_type.params[0];
            if let Type::Ref(inner) = param_type {
                if !can_coerce_type(receiver.ty(), inner.as_ref()) {
                    return CodeError::FunctionCallArgumentType(
                        callee_name.clone(),
                        inner.to_string(),
                        receiver.ty().to_string(),
                    )
                    .err(callee.1);
                }
            } else if !can_coerce_type(receiver.ty(), param_type) {
                return CodeError::FunctionCallArgumentType(
                    callee_name.clone(),
                    param_type.to_string(),
                    receiver.ty().to_string(),
                )
                .err(callee.1);
            }

            arg_operands.push(Operand::Place(receiver));
            1
        } else {
            0
        };

        for (arg, param_type) in args.iter().zip(func_type.params[param_offset..].iter()) {
            let (value, control) = self.lower_call_arg(&callee_name, arg, param_type)?;
            if matches!(control, Control::Terminates) {
                return Ok((LoweredValue::Never, Control::Terminates));
            }

            match value {
                LoweredValue::Value { operand, .. } => arg_operands.push(operand),
                LoweredValue::Void => {
                    return CodeError::FunctionCallArgumentType(
                        callee_name.clone(),
                        param_type.to_string(),
                        Type::Void.to_string(),
                    )
                    .err(arg.1);
                }
                LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
            }
        }

        let callee = MirCallee(func_id);
        match *func_type.return_type {
            Type::Void => {
                self.push_statement(MirStatement::Eval(Rvalue::Call {
                    callee,
                    args: arg_operands,
                }));
                Ok((LoweredValue::Void, Control::Continues))
            }
            Type::Never => {
                self.push_statement(MirStatement::Eval(Rvalue::Call {
                    callee,
                    args: arg_operands,
                }));
                self.set_terminator(MirTerminator::Unreachable);
                Ok((LoweredValue::Never, Control::Terminates))
            }
            return_type => {
                let temp = self.new_local(MirLocal {
                    name: None,
                    ty: return_type.clone(),
                    kind: LocalKind::Temp,
                });
                let temp_place = Place::Local(temp, return_type.clone());

                self.push_statement(MirStatement::Assign(
                    temp_place.clone(),
                    Rvalue::Call {
                        callee,
                        args: arg_operands,
                    },
                ));

                Ok((
                    LoweredValue::Value {
                        operand: Operand::Place(temp_place),
                    },
                    Control::Continues,
                ))
            }
        }
    }

    fn lower_call_arg(
        &mut self,
        callee_name: &str,
        arg: &Spanned<Arg>,
        param_type: &Type,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        if let Type::Ref(inner) = param_type {
            if !arg.0.is_ref {
                return CodeError::ExpectedRefArgument.err(arg.0.expr.1);
            }

            let place = self.lower_place(&arg.0.expr)?;
            if !can_coerce_type(place.ty(), inner.as_ref()) {
                return CodeError::FunctionCallArgumentType(
                    callee_name.to_owned(),
                    inner.to_string(),
                    place.ty().to_string(),
                )
                .err(arg.0.expr.1);
            }

            Ok((
                LoweredValue::Value {
                    operand: Operand::Place(place),
                },
                Control::Continues,
            ))
        } else {
            if arg.0.is_ref {
                return CodeError::UnExpectedRefArgument.err(arg.0.expr.1);
            }

            let (value, control) = self.lower_expr(&arg.0.expr, Some(param_type.clone()))?;
            if matches!(control, Control::Terminates) {
                return Ok((LoweredValue::Never, Control::Terminates));
            }

            let ty = value.ty();
            if !can_coerce_type(&ty, param_type) {
                return CodeError::FunctionCallArgumentType(
                    callee_name.to_owned(),
                    param_type.to_string(),
                    ty.to_string(),
                )
                .err(arg.0.expr.1);
            }

            Ok((value, Control::Continues))
        }
    }

    fn resolve_expr_as_symbol(&self, expr: &Spanned<Expression>) -> Option<&LowerSymbol> {
        match &expr.0 {
            Expression::Identifier(name) => match self.state.current_scope.get_symbol(name) {
                LookupResult::Found(
                    sym @ (LowerSymbol::Module(_)
                    | LowerSymbol::Type(_)
                    | LowerSymbol::Function(..)),
                ) => Some(sym),
                _ => None,
            },
            Expression::MemberAccess(base, member) => match self.resolve_expr_as_symbol(base)? {
                LowerSymbol::Module(map) => map.get(&member.0),
                LowerSymbol::Type(Type::Struct(type_id) | Type::Enum(type_id)) => self
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

    fn resolve_method_receiver_type(&self, receiver_expr: &Spanned<Expression>) -> Option<Type> {
        Some(self.lower_place(receiver_expr).ok()?.ty().clone())
    }

    fn resolve_method_call(
        &self,
        receiver_expr: &Spanned<Expression>,
        method_name: &str,
    ) -> Option<(LowerFuncId, FuncType)> {
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
            LowerSymbol::Function(func_id, func_type) => Some((*func_id, func_type.clone())),
            _ => None,
        }
    }

    fn lower_intrinsic(
        &mut self,
        intrinsic: &Intrinsic,
        args: &[Spanned<Expression>],
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        match intrinsic {
            Intrinsic::Print => self.lower_print_intrinsic(args, span, false),
            Intrinsic::Println => self.lower_print_intrinsic(args, span, true),
            Intrinsic::Assert => self.lower_assert_intrinsic(args, span),
            Intrinsic::Exit => self.lower_exit_intrinsic(args, span),
            Intrinsic::EnumToInt => self.lower_enum_to_int_intrinsic(args, span),
        }
    }

    fn lower_print_intrinsic(
        &mut self,
        args: &[Spanned<Expression>],
        span: SimpleSpan,
        new_line: bool,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let Some((format_arg, other_args)) = args.split_first() else {
            return CodeError::IntrinsicTooFewArguments("print", 1, args.len()).err(span);
        };

        let Expression::String(format_string) = &format_arg.0 else {
            return CodeError::ExpectedStringLiteral.err(format_arg.1);
        };

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

        let mut operands = Vec::with_capacity(args.len());
        let mut format_string = format_string.clone();
        if new_line {
            format_string.push('\n');
        }
        operands.push(Operand::Const(
            MirConst::String(format_string),
            Type::String,
        ));

        for arg in other_args {
            let (value, control) = self.lower_expr(arg, None)?;
            if matches!(control, Control::Terminates) {
                return Ok((LoweredValue::Never, Control::Terminates));
            }

            match value {
                LoweredValue::Value { operand } => {
                    let ty = operand.ty().clone();
                    if !PRINT_ARG_TYPES.contains(&ty) && !matches!(ty, Type::Enum(_)) {
                        return CodeError::IntrisicArgumentType(
                            "print",
                            &PRINT_ARG_TYPE_NAMES,
                            ty.to_string(),
                        )
                        .err(arg.1);
                    }
                    operands.push(operand);
                }
                LoweredValue::Void => {
                    return CodeError::IntrisicArgumentType(
                        "print",
                        &PRINT_ARG_TYPE_NAMES,
                        Type::Void.to_string(),
                    )
                    .err(arg.1);
                }
                LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
                LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
            }
        }

        let temp = self.new_local(MirLocal {
            name: None,
            ty: Type::I32,
            kind: LocalKind::Temp,
        });
        let temp_place = Place::Local(temp, Type::I32);

        self.push_statement(MirStatement::Assign(
            temp_place.clone(),
            Rvalue::Intrinsic {
                intrinsic: if new_line {
                    Intrinsic::Println
                } else {
                    Intrinsic::Print
                },
                args: operands,
            },
        ));

        Ok((
            LoweredValue::Value {
                operand: Operand::Place(temp_place),
            },
            Control::Continues,
        ))
    }

    fn lower_assert_intrinsic(
        &mut self,
        args: &[Spanned<Expression>],
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("assert", 1, args.len()).err(span);
        }

        let (value, control) = self.lower_expr(&args[0], Some(Type::Bool))?;
        if matches!(control, Control::Terminates) {
            return Ok((LoweredValue::Never, Control::Terminates));
        }

        let operand = match value {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                if !can_coerce_type(&ty, &Type::Bool) {
                    return CodeError::IntrisicArgumentType("assert", &["bool"], ty.to_string())
                        .err(args[0].1);
                }
                operand
            }
            LoweredValue::Void => {
                return CodeError::IntrisicArgumentType(
                    "assert",
                    &["bool"],
                    Type::Void.to_string(),
                )
                .err(args[0].1);
            }
            LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
            LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
        };

        self.push_statement(MirStatement::Eval(Rvalue::Intrinsic {
            intrinsic: Intrinsic::Assert,
            args: vec![operand],
        }));

        Ok((LoweredValue::Void, Control::Continues))
    }

    fn lower_exit_intrinsic(
        &mut self,
        args: &[Spanned<Expression>],
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("exit", 1, args.len()).err(span);
        }

        let (value, control) = self.lower_expr(&args[0], Some(Type::I32))?;
        if matches!(control, Control::Terminates) {
            return Ok((LoweredValue::Never, Control::Terminates));
        }

        let operand = match value {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                if !can_coerce_type(&ty, &Type::I32) {
                    return CodeError::IntrisicArgumentType("exit", &["i32"], ty.to_string())
                        .err(args[0].1);
                }
                operand
            }
            LoweredValue::Void => {
                return CodeError::IntrisicArgumentType("exit", &["i32"], Type::Void.to_string())
                    .err(args[0].1);
            }
            LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
            LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
        };

        self.push_statement(MirStatement::Eval(Rvalue::Intrinsic {
            intrinsic: Intrinsic::Exit,
            args: vec![operand],
        }));
        self.set_terminator(MirTerminator::Unreachable);

        Ok((LoweredValue::Never, Control::Terminates))
    }

    fn lower_enum_to_int_intrinsic(
        &mut self,
        args: &[Spanned<Expression>],
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        if args.len() != 1 {
            return CodeError::IntrinsicArgumentCount("enumToInt", 1, args.len()).err(span);
        }

        let (value, control) = self.lower_expr(&args[0], None)?;
        if matches!(control, Control::Terminates) {
            return Ok((LoweredValue::Never, Control::Terminates));
        }

        let operand = match value {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                if !matches!(ty, Type::Enum(_)) {
                    return CodeError::IntrinsicExpectedEnum("enumToInt", ty.to_string())
                        .err(args[0].1);
                }
                operand
            }
            LoweredValue::Void => {
                return CodeError::IntrinsicExpectedEnum("enumToInt", Type::Void.to_string())
                    .err(args[0].1);
            }
            LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
            LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
        };

        let temp = self.new_local(MirLocal {
            name: None,
            ty: Type::I32,
            kind: LocalKind::Temp,
        });
        let temp_place = Place::Local(temp, Type::I32);

        self.push_statement(MirStatement::Assign(
            temp_place.clone(),
            Rvalue::Intrinsic {
                intrinsic: Intrinsic::EnumToInt,
                args: vec![operand],
            },
        ));

        Ok((
            LoweredValue::Value {
                operand: Operand::Place(temp_place),
            },
            Control::Continues,
        ))
    }

    fn lower_struct_init(
        &mut self,
        struct_init: &StructInit,
        span: SimpleSpan,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let mut errors = ManyErrors::new();
        let mut arg_values = HashMap::with_capacity(struct_init.args.len());

        let path = &struct_init.path;
        let path_span = combine_spans(
            path.first()
                .expect("struct init path must contain at least one segment")
                .1,
            path.last()
                .expect("struct init path must contain at least one segment")
                .1,
        );
        let ty = self.type_from_path(path);

        for arg in &struct_init.args {
            let expected_type = ty
                .as_ref()
                .and_then(|ty| self.state.types.struct_type(ty))
                .and_then(|struct_type| struct_type.fields.get(&arg.0.0).cloned())
                .map(|struct_field| struct_field.ty);

            match self.lower_expr(&arg.1, expected_type) {
                Ok((value, control)) => {
                    if matches!(control, Control::Terminates) {
                        return Ok((LoweredValue::Never, Control::Terminates));
                    }

                    if arg_values
                        .insert(arg.0.0.clone(), (arg.0.1, value))
                        .is_some()
                    {
                        errors.push(
                            CodeError::StructInitArgDuplicate(arg.0.0.clone())
                                .spanned(combine_spans(arg.0.1, arg.1.1)),
                        );
                    }
                }
                Err(mut new_errors) => errors.append(&mut new_errors),
            }
        }

        let Some(ty) = ty else {
            let type_name = path
                .iter()
                .map(|segment| segment.0.as_str())
                .collect::<Vec<_>>()
                .join(".");
            errors.push(CodeError::UnknownType(type_name).spanned(path_span));
            return Err(errors);
        };

        let Type::Struct(struct_id) = ty else {
            errors.push(CodeError::NotStructType(ty.to_string()).spanned(path_span));
            return Err(errors);
        };

        if !errors.is_empty() {
            return Err(errors);
        }

        let struct_type = &self.state.types.structs[&struct_id];
        let struct_name = struct_type.name.to_string();
        let struct_fields = struct_type.fields.clone();
        let mut fields = Vec::with_capacity(struct_fields.len());
        let mut poison = LoweredValue::Void;

        for (field_index, (name, field)) in struct_fields.iter().enumerate() {
            let Some((_, value)) = arg_values.remove(name) else {
                errors.push(CodeError::StructInitMissingArg(name.clone()).spanned(span));
                continue;
            };

            let operand = match value {
                LoweredValue::Value { operand } => {
                    let ty = operand.ty().clone();
                    if !can_coerce_type(&ty, &field.ty) {
                        errors.push(
                            CodeError::StructInitInvalidType(field.ty.to_string(), ty.to_string())
                                .spanned(span),
                        );
                        continue;
                    }

                    operand
                }
                LoweredValue::Void => {
                    errors.push(
                        CodeError::StructInitInvalidType(
                            field.ty.to_string(),
                            Type::Void.to_string(),
                        )
                        .spanned(span),
                    );
                    continue;
                }
                LoweredValue::Never => {
                    poison = LoweredValue::Never;
                    continue;
                }
                LoweredValue::Error => {
                    poison = LoweredValue::Error;
                    continue;
                }
            };

            fields.push((FieldId(field_index), operand));
        }

        for (name, (field_span, _)) in arg_values {
            errors.push(CodeError::UnknownField(struct_name.clone(), name).spanned(field_span));
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        match poison {
            LoweredValue::Never => return Ok((LoweredValue::Never, Control::Terminates)),
            LoweredValue::Error => return Ok((LoweredValue::Error, Control::Continues)),
            LoweredValue::Void | LoweredValue::Value { .. } => {}
        }

        let ty = Type::Struct(struct_id);
        let temp = self.new_local(MirLocal {
            name: None,
            ty: ty.clone(),
            kind: LocalKind::Temp,
        });
        let temp_place = Place::Local(temp, ty.clone());

        self.push_statement(MirStatement::Assign(
            temp_place.clone(),
            Rvalue::StructInit {
                ty: struct_id,
                fields,
            },
        ));

        Ok((
            LoweredValue::Value {
                operand: Operand::Place(temp_place),
            },
            Control::Continues,
        ))
    }

    fn type_from_path(&self, path: &[Spanned<String>]) -> Option<Type> {
        if path.len() == 1 {
            return self.type_from_name(&path[0].0);
        }

        let LookupResult::Found(LowerSymbol::Module(map)) =
            self.state.current_scope.get_symbol(&path[0].0)
        else {
            return None;
        };
        let mut map = map;

        for segment in &path[1..path.len() - 1] {
            match map.get(&segment.0)? {
                LowerSymbol::Module(next_map) => map = next_map,
                _ => return None,
            }
        }

        match map.get(&path.last()?.0)? {
            LowerSymbol::Type(ty) => Some(ty.clone()),
            _ => None,
        }
    }

    fn type_from_name(&self, type_name: &str) -> Option<Type> {
        match type_name {
            "i8" => Some(Type::I8),
            "i16" => Some(Type::I16),
            "i32" => Some(Type::I32),
            "i64" => Some(Type::I64),
            "u8" => Some(Type::U8),
            "u16" => Some(Type::U16),
            "u32" => Some(Type::U32),
            "u64" => Some(Type::U64),
            "f32" => Some(Type::F32),
            "f64" => Some(Type::F64),
            "bool" => Some(Type::Bool),
            "string" => Some(Type::String),
            "void" => Some(Type::Void),
            name => self.state.current_scope.get_type(name).cloned(),
        }
    }

    fn lower_while(
        &mut self,
        conditional: &luga_syntax::Conditional,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let body_block = self.new_block_labelled("while_body");
        let exit_block = self.new_block();
        let condition_block = self.new_block_labelled("while_condition");

        self.set_terminator(MirTerminator::Jump(condition_block));
        self.switch_to_block(condition_block);

        let condition_value = self.lower_expr(&conditional.condition, None)?;

        match condition_value.0 {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                if !can_coerce_type(&ty, &Type::Bool) {
                    return CodeError::InvalidConditionType(ty.to_string())
                        .err(conditional.condition.1);
                }

                self.set_terminator(MirTerminator::Branch {
                    cond: operand,
                    then_block: body_block,
                    else_block: exit_block,
                });
            }
            LoweredValue::Void => {
                return CodeError::InvalidConditionType(Type::Void.to_string())
                    .err(conditional.condition.1);
            }
            LoweredValue::Never => {
                return Ok((LoweredValue::Never, Control::Terminates));
            }
            LoweredValue::Error => {
                self.set_terminator(MirTerminator::Jump(exit_block));
                self.switch_to_block(exit_block);
                return Ok((LoweredValue::Void, Control::Continues));
            }
        }

        self.switch_to_block(body_block);
        let body = self.lower_block_body(&conditional.block, None)?;

        if !matches!(body.1, Control::Terminates) {
            self.set_terminator(MirTerminator::Jump(condition_block));
        }

        self.switch_to_block(exit_block);

        Ok((LoweredValue::Void, Control::Continues))
    }

    fn lower_if(
        &mut self,
        if_expr: &If,
        span: SimpleSpan,
        try_into_type: Option<Type>,
    ) -> Result<(LoweredValue, Control), ManyErrors> {
        let mut errors = ManyErrors::new();
        let join_block = self.new_block_labelled("if_join");

        let condition_blocks = if_expr
            .if_branches
            .iter()
            .enumerate()
            .map(|(idx, _)| self.new_block_labelled(format!("if_condition_{idx}")))
            .collect::<Vec<_>>();
        let branch_blocks = if_expr
            .if_branches
            .iter()
            .enumerate()
            .map(|(idx, _)| self.new_block_labelled(format!("if_branch_{idx}")))
            .collect::<Vec<_>>();
        let else_block = if if_expr.else_branch.is_some() {
            Some(self.new_block_labelled("if_else"))
        } else {
            None
        };

        let Some(first_condition_block) = condition_blocks.first().copied() else {
            return Ok((LoweredValue::Void, Control::Continues));
        };

        self.set_terminator(MirTerminator::Jump(first_condition_block));

        let mut result_type = match try_into_type.clone() {
            Some(Type::Void | Type::Never | Type::Error) | None => None,
            Some(ty) => Some(ty),
        };
        let mut result_local = result_type.clone().map(|ty| {
            self.new_local(MirLocal {
                name: None,
                ty,
                kind: LocalKind::Temp,
            })
        });
        let mut has_continuing_path = if_expr.else_branch.is_none();
        let mut has_value_branch = false;
        let mut has_void_branch = false;

        for (idx, conditional) in if_expr.if_branches.iter().enumerate() {
            let condition_block = condition_blocks[idx];
            let branch_block = branch_blocks[idx];
            let next_block = condition_blocks
                .get(idx + 1)
                .copied()
                .or(else_block)
                .unwrap_or(join_block);

            self.switch_to_block(condition_block);
            match self.lower_expr(&conditional.condition, Some(Type::Bool)) {
                Ok((LoweredValue::Value { operand }, _)) => {
                    let ty = operand.ty().clone();
                    if !can_coerce_type(&ty, &Type::Bool) {
                        errors.push(
                            CodeError::InvalidConditionType(ty.to_string())
                                .spanned(conditional.condition.1),
                        );
                        self.set_terminator(MirTerminator::Jump(next_block));
                    } else {
                        self.set_terminator(MirTerminator::Branch {
                            cond: operand,
                            then_block: branch_block,
                            else_block: next_block,
                        });
                    }
                }
                Ok((LoweredValue::Void, _)) => {
                    errors.push(
                        CodeError::InvalidConditionType(Type::Void.to_string())
                            .spanned(conditional.condition.1),
                    );
                    self.set_terminator(MirTerminator::Jump(next_block));
                }
                Ok((LoweredValue::Never, _)) => {}
                Ok((LoweredValue::Error, _)) => {
                    self.set_terminator(MirTerminator::Jump(next_block));
                }
                Err(mut new_errors) => {
                    errors.append(&mut new_errors);
                    self.set_terminator(MirTerminator::Jump(next_block));
                }
            }

            self.switch_to_block(branch_block);
            match self.lower_if_branch(
                &conditional.block,
                try_into_type.clone(),
                &mut result_type,
                &mut result_local,
                span,
            ) {
                Ok((control, branch_result)) => {
                    has_value_branch |= matches!(branch_result, BranchResult::Value);
                    has_void_branch |= matches!(branch_result, BranchResult::Void);
                    if matches!(control, Control::Continues) {
                        has_continuing_path = true;
                        self.set_terminator(MirTerminator::Jump(join_block));
                    }
                }
                Err(mut new_errors) => errors.append(&mut new_errors),
            }
        }

        if let Some(else_branch) = &if_expr.else_branch {
            let else_block = else_block.expect("else block must exist for else branch");
            self.switch_to_block(else_block);
            match self.lower_if_branch(
                else_branch,
                try_into_type.clone(),
                &mut result_type,
                &mut result_local,
                span,
            ) {
                Ok((control, branch_result)) => {
                    has_value_branch |= matches!(branch_result, BranchResult::Value);
                    has_void_branch |= matches!(branch_result, BranchResult::Void);
                    if matches!(control, Control::Continues) {
                        has_continuing_path = true;
                        self.set_terminator(MirTerminator::Jump(join_block));
                    }
                }
                Err(mut new_errors) => errors.append(&mut new_errors),
            }
        } else if try_into_type.is_some() || has_value_branch {
            errors.push(CodeError::MissingElseBranch.spanned(span));
        }

        if !errors.is_empty() {
            self.switch_to_block(join_block);
            return Err(errors);
        }

        if has_value_branch && has_void_branch {
            self.switch_to_block(join_block);
            return Err(CodeError::IfExpressionType.spanned(span).into());
        }

        if !has_continuing_path {
            return Ok((LoweredValue::Never, Control::Terminates));
        }

        self.switch_to_block(join_block);

        if let (Some(local), Some(ty)) = (result_local, result_type) {
            Ok((
                LoweredValue::Value {
                    operand: Operand::Place(Place::Local(local, ty.clone())),
                },
                Control::Continues,
            ))
        } else {
            Ok((LoweredValue::Void, Control::Continues))
        }
    }

    fn lower_if_branch(
        &mut self,
        statements: &[Spanned<Statement>],
        try_into_type: Option<Type>,
        result_type: &mut Option<Type>,
        result_local: &mut Option<LocalId>,
        span: SimpleSpan,
    ) -> Result<(Control, BranchResult), ManyErrors> {
        let result = self.lower_block_body(statements, try_into_type);
        let (value, control) = result?;

        if matches!(control, Control::Terminates) {
            return Ok((control, BranchResult::NoContinue));
        }

        match value {
            LoweredValue::Value { operand } => {
                let ty = operand.ty().clone();
                if result_type.is_none() {
                    *result_type = Some(ty.clone());
                }

                let expected_type = result_type
                    .as_ref()
                    .expect("if result type must be known for value branch")
                    .clone();

                if !can_coerce_type(&ty, &expected_type) {
                    return Err(CodeError::IfExpressionType.spanned(span).into());
                }

                let local = match result_local {
                    Some(local) => *local,
                    None => {
                        let local = self.new_local(MirLocal {
                            name: None,
                            ty: expected_type.clone(),
                            kind: LocalKind::Temp,
                        });
                        *result_local = Some(local);
                        local
                    }
                };

                self.push_statement(MirStatement::Assign(
                    Place::Local(local, expected_type),
                    Rvalue::Use(operand),
                ));

                Ok((control, BranchResult::Value))
            }
            LoweredValue::Void => Ok((control, BranchResult::Void)),
            LoweredValue::Error => Ok((control, BranchResult::NoContinue)),
            LoweredValue::Never => Ok((Control::Terminates, BranchResult::NoContinue)),
        }
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

#[derive(Debug, Clone)]
pub struct LocalScope {
    scopes: Vec<HashMap<String, LocalId>>,
}

impl Default for LocalScope {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalScope {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub fn exit_scope(&mut self) {
        let popped = self.scopes.pop();
        debug_assert!(popped.is_some());

        if self.scopes.is_empty() {
            self.scopes.push(HashMap::new());
        }
    }

    pub fn define(&mut self, name: String, local: LocalId) -> Option<LocalId> {
        self.current_scope_mut().insert(name, local)
    }

    pub fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    pub fn contains_in_current_scope(&self, name: &str) -> bool {
        self.current_scope().contains_key(name)
    }

    fn current_scope(&self) -> &HashMap<String, LocalId> {
        self.scopes
            .last()
            .expect("local scope must always contain at least one frame")
    }

    fn current_scope_mut(&mut self) -> &mut HashMap<String, LocalId> {
        self.scopes
            .last_mut()
            .expect("local scope must always contain at least one frame")
    }
}
