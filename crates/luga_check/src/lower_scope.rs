use crate::lower_state::LowerFuncId;
use crate::types::{FuncType, Type};
use std::collections::HashMap;

pub struct LowerScope {
    pub(crate) symbols: HashMap<String, LowerSymbol>,
    pub parent: Option<Box<LowerScope>>,
    is_function_boundary: bool,
}

#[derive(Clone, Debug)]
pub enum LowerSymbol {
    Local(Type),
    Function(LowerFuncId, FuncType),
    Type(Type),
    Module(HashMap<String, LowerSymbol>),
}

pub enum LookupResult<'a> {
    Found(&'a LowerSymbol),
    CaptureError,
    NotFound,
}

impl LowerScope {
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
            Some(LowerSymbol::Local(..)) if crossed_boundary => return LookupResult::CaptureError,
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

    pub fn insert_symbol(&mut self, name: String, symbol: LowerSymbol) -> Result<(), ()> {
        use std::collections::hash_map::Entry;

        match self.symbols.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(symbol);
                Ok(())
            }
            Entry::Occupied(_) => Err(()),
        }
    }

    pub fn get_function(&self, name: &str) -> Option<(LowerFuncId, &FuncType)> {
        match self.get_symbol(name) {
            LookupResult::Found(LowerSymbol::Function(id, ft)) => Some((*id, ft)),
            _ => None,
        }
    }

    pub fn get_type(&self, name: &str) -> Option<&Type> {
        match self.get_symbol(name) {
            LookupResult::Found(LowerSymbol::Type(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_module(&self, name: &str) -> Option<&HashMap<String, LowerSymbol>> {
        match self.get_symbol(name) {
            LookupResult::Found(LowerSymbol::Module(m)) => Some(m),
            _ => None,
        }
    }

    pub fn get_module_symbol(&self, module_name: &str, symbol_name: &str) -> Option<&LowerSymbol> {
        self.get_module(module_name)?.get(symbol_name)
    }
}
