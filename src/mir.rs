use luga_syntax::parser::{BinOp, UnOp};

use crate::lower_state::LowerFuncId;
use crate::types::{Type, TypeId};
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display, Write},
};

#[derive(Debug, Clone)]
pub struct MirProgram(pub Vec<MirFunction>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub usize);

#[derive(Debug, Clone, PartialEq)]
pub struct MirFunction {
    pub name: String,
    pub params: Vec<LocalId>,
    pub locals: Vec<MirLocal>,
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub return_type: Type,
    pub type_names: HashMap<TypeId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirLocal {
    pub name: Option<String>,
    pub ty: Type,
    pub kind: LocalKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    Param,
    User,
    Temp,
    ReturnSlot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BasicBlock {
    pub label: Option<String>,
    pub statements: Vec<MirStatement>,
    pub terminator: MirTerminator,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MirStatement {
    Assign(Place, Rvalue),
    Eval(Rvalue),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rvalue {
    Use(Operand),
    Binary {
        op: BinOp,
        left: Operand,
        right: Operand,
    },
    Unary {
        op: UnOp,
        value: Operand,
    },
    Call {
        callee: MirCallee,
        args: Vec<Operand>,
    },
    Intrinsic {
        intrinsic: MirIntrinsic,
        args: Vec<Operand>,
    },
    StructInit {
        ty: Type,
        fields: Vec<(FieldId, Operand)>,
    },
    EnumVariant {
        ty: Type,
        variant: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MirTerminator {
    Return(Option<Operand>),
    Jump(BlockId),
    Branch {
        cond: Operand,
        then_block: BlockId,
        else_block: BlockId,
    },
    Unreachable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Place(Place),
    Const(MirConst),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MirConst {
    Bool(bool),
    Float(f32),
    Int(i64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirIntrinsic {
    Print,
    Println,
    Assert,
    Exit,
    EnumToInt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Local(LocalId, Type),
    Field {
        base: Box<Place>,
        field: FieldId,
        ty: Type,
    },
    Deref {
        pointer: Box<Place>,
        ty: Type,
    },
}
impl Place {
    pub fn ty(&self) -> &Type {
        match self {
            Place::Local(_, ty) | Place::Field { ty, .. } | Place::Deref { ty, .. } => ty,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MirCallee {
    Function(LowerFuncId),
    Operand(Operand),
}

impl Display for MirFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let params = self
            .params
            .iter()
            .map(|param| format!("%{}", param.0))
            .collect::<Vec<_>>()
            .join(", ");

        writeln!(
            f,
            "fn {}({params}) -> {} {{",
            self.name,
            self.fmt_type(&self.return_type)
        )?;

        let mut assigned_locals = HashSet::new();

        for (idx, block) in self.blocks.iter().enumerate() {
            let label = block
                .label
                .as_ref()
                .map(|label| format!(" ({label})"))
                .unwrap_or_default();
            writeln!(f, "  bb{idx}{label}:")?;

            for statement in &block.statements {
                write!(f, "    ")?;
                self.fmt_statement(f, statement, &mut assigned_locals)?;
                writeln!(f)?;
            }
            writeln!(f, "    {}", block.terminator)?;

            if idx + 1 != self.blocks.len() {
                writeln!(f)?;
            }
        }

        write!(f, "}}")
    }
}

impl MirFunction {
    fn fmt_statement(
        &self,
        f: &mut fmt::Formatter<'_>,
        statement: &MirStatement,
        assigned_locals: &mut HashSet<LocalId>,
    ) -> fmt::Result {
        match statement {
            MirStatement::Assign(place, rvalue) => {
                write!(f, "{place} = ")?;
                self.fmt_rvalue(f, rvalue)?;
                if let Place::Local(local, _) = place
                    && assigned_locals.insert(*local)
                    && let Some(local_info) = self.locals.get(local.0)
                {
                    if let Some(name) = &local_info.name {
                        write!(
                            f,
                            "        {name}: {} {:?}",
                            self.fmt_type(&local_info.ty),
                            local_info.kind
                        )?;
                    } else {
                        write!(
                            f,
                            "        {} {:?}",
                            self.fmt_type(&local_info.ty),
                            local_info.kind
                        )?;
                    }
                }
                Ok(())
            }
            MirStatement::Eval(rvalue) => self.fmt_rvalue(f, rvalue),
        }
    }

    fn fmt_rvalue(&self, f: &mut fmt::Formatter<'_>, rvalue: &Rvalue) -> fmt::Result {
        match rvalue {
            Rvalue::StructInit { ty, fields } => {
                write!(f, "{} {{ ", self.fmt_type(ty))?;
                for (idx, (field, operand)) in fields.iter().enumerate() {
                    if idx != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, ".{} = {operand}", field.0)?;
                }
                write!(f, " }}")
            }
            Rvalue::EnumVariant { ty, variant } => write!(f, "{}.{variant}", self.fmt_type(ty)),
            rvalue => write!(f, "{rvalue}"),
        }
    }

    fn fmt_type<'a>(&'a self, ty: &'a Type) -> DisplayMirType<'a> {
        DisplayMirType {
            ty,
            type_names: &self.type_names,
        }
    }
}

struct DisplayMirType<'a> {
    ty: &'a Type,
    type_names: &'a HashMap<TypeId, String>,
}

impl Display for DisplayMirType<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            Type::Struct(id) | Type::Enum(id) => match self.type_names.get(id) {
                Some(name) => write!(f, "{name}"),
                None => write!(f, "{}", self.ty),
            },
            Type::Ref(ty) => write!(
                f,
                "ref<{}>",
                DisplayMirType {
                    ty,
                    type_names: self.type_names,
                }
            ),
            Type::Fn(func_type) => write!(f, "{func_type}"),
            ty => write!(f, "{ty}"),
        }
    }
}

impl Display for MirStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirStatement::Assign(place, rvalue) => write!(f, "{place} = {rvalue}"),
            MirStatement::Eval(rvalue) => write!(f, "{rvalue}"),
        }
    }
}

impl Display for Rvalue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rvalue::Use(operand) => write!(f, "{operand}"),
            Rvalue::Binary { op, left, right } => write!(f, "{left} {op} {right}"),
            Rvalue::Unary { op, value } => write!(f, "{op}{value}"),
            Rvalue::Call { callee, args } => write!(f, "call {callee}({})", display_list(args)),
            Rvalue::Intrinsic { intrinsic, args } => {
                write!(f, "@{intrinsic}({})", display_list(args))
            }
            Rvalue::StructInit { ty, fields } => {
                write!(f, "{ty} {{ ")?;
                for (idx, (field, operand)) in fields.iter().enumerate() {
                    if idx != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, ".{} = {operand}", field.0)?;
                }
                write!(f, " }}")
            }
            Rvalue::EnumVariant { ty, variant } => write!(f, "{ty}.{variant}"),
        }
    }
}

impl Display for MirTerminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirTerminator::Return(Some(value)) => write!(f, "return {value}"),
            MirTerminator::Return(None) => write!(f, "return"),
            MirTerminator::Jump(block) => write!(f, "jump {block}"),
            MirTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => write!(f, "branch {cond}, {then_block}, {else_block}"),
            MirTerminator::Unreachable => write!(f, "unreachable"),
        }
    }
}

impl Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Place(place) => write!(f, "{place}"),
            Operand::Const(value) => write!(f, "{value}"),
        }
    }
}

impl Display for MirConst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirConst::Bool(value) => write!(f, "{value}"),
            MirConst::Float(value) => write!(f, "{value}"),
            MirConst::Int(value) => write!(f, "{value}"),
            MirConst::String(value) => write!(f, "{value:?}"),
        }
    }
}

impl Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Local(local, _) => write!(f, "{local}"),
            Place::Field { base, field, .. } => write!(f, "{base}.{field}"),
            Place::Deref { pointer, .. } => write!(f, "*{pointer}"),
        }
    }
}

impl Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

impl Display for FieldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "field{}", self.0)
    }
}

impl Display for MirIntrinsic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirIntrinsic::Print => write!(f, "print"),
            MirIntrinsic::Println => write!(f, "println"),
            MirIntrinsic::Assert => write!(f, "assert"),
            MirIntrinsic::Exit => write!(f, "exit"),
            MirIntrinsic::EnumToInt => write!(f, "enumToInt"),
        }
    }
}

impl Display for MirCallee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirCallee::Function(func_id) => write!(f, "{func_id:?}"),
            MirCallee::Operand(operand) => write!(f, "{operand}"),
        }
    }
}

fn display_list<T: Display>(items: &[T]) -> String {
    let mut out = String::new();
    for (idx, item) in items.iter().enumerate() {
        if idx != 0 {
            out.push_str(", ");
        }
        write!(&mut out, "{item}").expect("writing to string must not fail");
    }
    out
}
