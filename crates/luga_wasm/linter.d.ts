export function load(): Promise<{
	check(main: string, files: Record<string, string>): Diagnostic[]
}>

export type Diagnostic = {
	path: string      // a key from `files`, exactly as the consumer passed it
	from: number      // UTF-16 offsets into files[path]
	to: number        // always >= from
	message: string
	severity: "error"
}