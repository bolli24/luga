# luga

Compiled, statically typed language written in Rust, with a Cranelift backend.

Try it in the browser: https://henrikhuth.de/playground/?lang=luga

- Pipeline: lexer/parser → type checker → MIR → Cranelift IR → native object file, linked with `gcc`
- Structs with associated functions, enums, `ref` parameters, modules/imports
- `extern fn` for calling C functions (ABI notes in `EXTERN.MD`)
- Targets x86_64 Windows and Linux

## Layout

- `crates/luga_syntax`: lexer and parser
- `crates/luga_check`: type checking and lowering to MIR
- `crates/luga_codegen`: Cranelift code generation
- `crates/luga_wasm`: checker compiled to Wasm, used as the playground linter
- `lsp`: language server
- `runtime`: small runtime library linked into every program
- `playground`: image for the web playground

## Usage

```sh
luga run test/tour.luga
luga build file.luga --emit mir,clif,asm
luga check file.luga
luga test test/
```
