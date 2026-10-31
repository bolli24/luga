mod context;
mod display;

pub use context::TypeContext;

use indexmap::{IndexMap, IndexSet};

use crate::lower::ModuleId;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TypeId {
    local: usize,
    module: ModuleId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedName(pub ModuleId, pub String);

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
    Ref(Box<Type>),
}

impl Type {
    pub fn is_int(&self) -> bool {
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

    pub fn is_signed_int(&self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    pub fn is_unsigned_int(&self) -> bool {
        matches!(self, Self::U8 | Self::U16 | Self::U32 | Self::U64)
    }

    pub fn is_ref(&self) -> bool {
        matches!(self, Self::Ref(_))
    }

    pub fn is_enum(&self) -> bool {
        matches!(self, Self::Enum(_))
    }

    pub fn is_struct(&self) -> bool {
        matches!(self, Self::Struct(_))
    }

    pub fn ref_of(self) -> Option<Self> {
        match self {
            Self::Ref(ref_type) => {
                debug_assert!(!ref_type.is_ref());
                Some(*ref_type)
            }
            _ => None,
        }
    }

    pub fn into_ref(self) -> Self {
        Self::Ref(Box::new(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Box<Type>,
    pub is_extern: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructType {
    pub name: QualifiedName,
    pub fields: IndexMap<String, StructField>,
    pub size: u32,
    pub align_shift: u8,
}

impl StructType {
    pub fn new(
        name: QualifiedName,
        fields: IndexMap<String, StructField>,
        type_context: &TypeContext,
    ) -> Self {
        let align_shift = fields
            .values()
            .map(|f| type_context.align_shift(&f.ty))
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

    pub fn compute_size(
        fields: &IndexMap<String, StructField>,
        align_shift: u8,
        type_context: &TypeContext,
    ) -> u32 {
        let mut offset = 0;
        for field in fields.values() {
            let align = 1 << type_context.align_shift(&field.ty);
            let padding = (align - (offset % align)) % align;
            offset += padding;
            offset += type_context.size(&field.ty)
        }

        // Final padding so total size is a multiple of the struct's alignment
        let struct_align = 1 << align_shift;
        let padding = (struct_align - (offset % struct_align)) % struct_align;
        offset + padding
    }

    pub fn size_pad_8(&self) -> u32 {
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
    pub ty: Type,
    pub offset: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    pub name: QualifiedName,
    pub variants: IndexSet<String>,
}
