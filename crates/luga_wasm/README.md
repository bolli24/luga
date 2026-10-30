# luga_wasm

The luga checker compiled to WebAssembly, for showing errors in a browser editor such as CodeMirror.

`linter.js` exposes `load()`, which resolves to a checker with `check(main, files)`. See `linter.d.ts` for the
diagnostic format.

## Demo

`index.html` lints two files, `main.luga` and an imported module, in CodeMirror. From the repository root:

```sh
wasm-pack build crates/luga_wasm --target web
cd crates/luga_wasm
python -m http.server 8765
```

Then open <http://127.0.0.1:8765/>. The page must be served over HTTP, since browsers don't load wasm from `file://`.
