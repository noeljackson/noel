/**
 * The TS clients embed the host kernel release in their user-agent
 * (`pi (linux 7.1.5-arch1-2; x64)`), which makes fixtures drift on every
 * kernel upgrade and differ between hosts; pin it to a stable placeholder.
 */
export function stableUserAgent(userAgent: string): string {
	return userAgent.replace(/\b(linux|darwin|win32) [^;)]+;/, "$1 <kernel>;");
}
