/** An error with an HTTP status and a stable machine-readable code. */
export class ApiError extends Error {
	constructor(
		readonly status: number,
		readonly code: string,
		message?: string,
		readonly extra: Record<string, unknown> = {},
		readonly headers: Record<string, string> = {}
	) {
		super(message ?? code)
		this.name = 'ApiError'
	}
}
