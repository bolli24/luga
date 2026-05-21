use std::collections::HashMap;

use crate::{generator::Symbol, types::*};

pub struct TypeContext {
    pub structs: HashMap<TypeId, StructType>,
    pub enums: HashMap<TypeId, EnumType>,
    pub associated_functions: HashMap<TypeId, HashMap<String, Symbol>>,
    next_type_id: usize,
}

impl TypeContext {
    pub fn new() -> Self {
        Self {
            structs: HashMap::new(),
            enums: HashMap::new(),
            associated_functions: HashMap::new(),
            next_type_id: 0,
        }
    }

    pub fn alloc_type_id(&mut self, module: ModuleId) -> TypeId {
        let id = TypeId {
            local: self.next_type_id,
            module,
        };
        self.next_type_id += 1;
        id
    }

    pub fn type_name(&self, id: TypeId) -> String {
        if let Some(struct_type) = self.structs.get(&id) {
            return struct_type.name.to_string();
        }
        if let Some(enum_type) = self.enums.get(&id) {
            return enum_type.name.to_string();
        }
        format!("<type {id:?}>")
    }

    pub fn layout(&self, r#type: &Type) -> (u32, u8) {
        match r#type {
            Type::I8 | Type::U8 => (1, 0),
            Type::I16 | Type::U16 => (2, 1),
            Type::I32 | Type::U32 => (4, 2),
            Type::I64 | Type::U64 => (8, 3),
            Type::F32 => (4, 2),
            Type::F64 => (8, 3),
            Type::Void | Type::Never => (0, 0),
            Type::String | Type::Ref(_) => (8, 3), // TODO: platform pointer size
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

    pub fn size(&self, r#type: &Type) -> u32 {
        self.layout(r#type).0
    }

    pub fn align_shift(&self, r#type: &Type) -> u8 {
        self.layout(r#type).1
    }

    pub fn struct_type(&self, r#type: &Type) -> Option<&StructType> {
        match r#type {
            Type::Struct(struct_id) => self.structs.get(struct_id),
            _ => None,
        }
    }
}
