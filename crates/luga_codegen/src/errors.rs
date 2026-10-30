use cranelift::module::ModuleError as CraneModuleError;

pub enum GeneratorErrors {
    CraneModuleErrors(Vec<CraneModuleError>),
    MissingMain,
}
