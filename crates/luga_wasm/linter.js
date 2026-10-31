import init, { check as checkJson } from "./pkg/luga_wasm.js";

// Diagnostics have the shape { path, from, to, message, severity }, with `from` and
// `to` being UTF-16 offsets into files[path] as used by CodeMirror
export async function load() {
	await init();

	return {
		check(main, files) {
			try {
				return JSON.parse(checkJson(main, JSON.stringify(files)));
			} catch (e) {
				// A panic in the checker traps the wasm module. Report it instead of
				// taking the editor down; errors thrown on purpose (e.g. a missing main) propagate.
				if (!(e instanceof WebAssembly.RuntimeError)) throw e;
				return [{ path: main, from: 0, to: 0, message: `internal compiler error: ${e.message}`, severity: "error" }];
			}
		},
	};
}
