use std::fmt::Display;

use crate::types::*;

impl Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}

impl Type {
    pub fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayType<'a> {
        DisplayType { ty: self, types }
    }
}

pub struct DisplayType<'a> {
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
            Self::Ref(ref_type) => write!(f, "ref<{ref_type}>"),
        }
    }
}

impl FuncType {
    pub fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayFuncType<'a> {
        DisplayFuncType { func: self, types }
    }
}

pub struct DisplayFuncType<'a> {
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
    pub fn display<'a>(&'a self, types: &'a TypeContext) -> DisplayStructType<'a> {
        DisplayStructType {
            struct_type: self,
            types,
        }
    }
}

pub struct DisplayStructType<'a> {
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
    pub fn display<'a>(&'a self, _types: &'a TypeContext) -> DisplayEnumType<'a> {
        DisplayEnumType { enum_type: self }
    }
}

pub struct DisplayEnumType<'a> {
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
