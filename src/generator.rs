use cranelift::{
    codegen::{Context, verify_function},
    prelude::*,
};
use cranelift_module::Module;
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};

use crate::parser::Program;

pub fn generate(program: Program) -> Result<ObjectProduct, ()> {
    #[cfg(target_os = "linux")]
    const TARGET_TRIPLE: &str = "x86_64-unknown-linux";

    #[cfg(target_os = "windows")]
    const TARGET_TRIPLE: &str = "x86_64-pc-windows-gnu";

    let isa = {
        let mut builder = settings::builder();

        // disable optimizations so dissassembly will more directly correlated to our Cranelift usage
        builder.set("opt_level", "speed").unwrap();

        builder.enable("is_pic").unwrap();

        let flags = settings::Flags::new(builder);

        isa::lookup_by_name(TARGET_TRIPLE)
            .unwrap()
            .finish(flags)
            .unwrap()
    };

    let mut module = {
        let translation_unit_name = b"output_a_binary";
        let libcall_names = cranelift_module::default_libcall_names();
        let builder =
            ObjectBuilder::new(isa.clone(), translation_unit_name, libcall_names).unwrap();
        ObjectModule::new(builder)
    };
    let mut context = Context::new();

    let signature = Signature {
        call_conv: isa.default_call_conv(),
        params: vec![],
        // Since we're linking to libc, we can return the exit code from main.
        returns: vec![AbiParam::new(types::I32)],
    };

    let main_id = module
        .declare_function("main", cranelift_module::Linkage::Export, &signature)
        .unwrap();

    // let mut vars: IndexMap<String, Variable> = IndexMap::new();

    let mut function_context = FunctionBuilderContext::new();

    let mut builder = FunctionBuilder::new(&mut context.func, &mut function_context);
    builder.func.signature = signature;

    let main_block = builder.create_block();
    builder.switch_to_block(main_block);
    builder.seal_block(main_block);

    /* for statement in &program.statements {
            match statement {
                Statement::Expression(_expression) => todo!(),
                Statement::Binding(name, expression) => {
                    let var = Variable::new(vars.len());
                    builder.declare_var(var, F64);
                    vars.insert(name.clone(), var);

                    let Expression::Number(number) = expression else {
                        todo!();
                    };

                    let value = builder.ins().f64const(Ieee64::with_float(*number));

                    builder.def_var(var, value);
                }
                Statement::Assignment(_left, _right) => todo!(),
            }
        }
    */

    let zero = builder.ins().iconst(types::I32, 100);

    builder.ins().return_(&[zero]);

    builder.finalize();

    println!("{}", context.func.display());

    let flags = settings::Flags::new(settings::builder());
    if let Err(errs) = verify_function(&context.func, &flags) {
        for err in &errs.0 {
            eprintln!("{err}");
        }
    }

    module.define_function(main_id, &mut context).unwrap();

    Ok(module.finish())
}
