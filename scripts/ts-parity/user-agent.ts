/**
 * The TS clients embed the host kernel release in their user-agent
 * (`pi (linux 7.1.5-arch1-2; x64)`), which makes fixtures drift on every
 * kernel upgrade and differ between hosts; pin it to a stable placeholder.
 */
export function stableUserAgent(userAgent: string): string {
	// The runtime patch version (`node/v24.22.2`) churns on every Node
	// release without changing the request; pin it, as the kernel is.
	return userAgent
		.replace(/\b(linux|darwin|win32) [^;)]+;/, "$1 <kernel>;")
		.replace(/node\/v[\d.]+/, "node/<version>");
}
