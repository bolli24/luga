// Playground linter module, see "Linter" in CONTRACT.md in the playground-runner repo.
// Wraps the wasm-bindgen output of crates/luga_wasm, which the Dockerfile puts next to this file.

import init, { check as checkJson } from "./luga_wasm.js";

export default async function () {
	await init();

	return {
		check(files, entry) {
			try {
				return JSON.parse(checkJson(entry, JSON.stringify(files))).map((d) => ({
					file: d.path,
					from: d.from,
					to: d.to,
					severity: d.severity,
					message: d.message,
				}));
			} catch (e) {
				// The contract says check never throws: report panics and errors such as a missing entry
				// file as a diagnostic instead.
				return [{ file: entry, from: 0, to: 0, severity: "error", message: `internal linter error: ${e.message ?? e}` }];
			}
		},
	};
}
