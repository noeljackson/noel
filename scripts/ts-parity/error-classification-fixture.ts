import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { getOverflowPatterns, isContextOverflow } from "/ts-reference/packages/ai/src/utils/overflow.ts";
import { isRetryableAssistantError } from "/ts-reference/packages/ai/src/utils/retry.ts";
import type { AssistantMessage } from "/ts-reference/packages/ai/src/types.ts";

/**
 * Records how upstream classifies a failed model request by its error text:
 * retryable (`isRetryableAssistantError`) and context overflow
 * (`isContextOverflow`), with the pattern lists both use.
 *
 * Upstream exports only the overflow patterns. The other three lists are read
 * from the source of the two modules; the cases below would expose a list that
 * was read wrongly.
 */

const RETRY_SOURCE = "/ts-reference/packages/ai/src/utils/retry.ts";
const OVERFLOW_SOURCE = "/ts-reference/packages/ai/src/utils/overflow.ts";

/** The body of `const <name> = ...[ ... ];` in `source`. */
function arrayBody(source: string, name: string): string {
	const match = new RegExp(`const ${name} = [^\\[]*\\[([\\s\\S]*?)\\n\\]`).exec(source);
	if (!match) throw new Error(`no ${name} in upstream's source`);
	return match[1];
}

/** The string literals of an array of patterns, one per line. */
function stringPatterns(source: string, name: string): string[] {
	const patterns = arrayBody(source, name)
		.split("\n")
		.map((line) => /^\s*("(?:[^"\\]|\\.)*"),/.exec(line)?.[1])
		.filter((literal): literal is string => literal !== undefined)
		.map((literal) => JSON.parse(literal) as string);
	if (patterns.length === 0) throw new Error(`${name} has no patterns`);
	return patterns;
}

/** The regex literals of an array of patterns, one per line; all ignore case. */
function regexPatterns(source: string, name: string): string[] {
	const patterns = arrayBody(source, name)
		.split("\n")
		.map((line) => /^\s*\/(.+)\/i,/.exec(line)?.[1])
		.filter((pattern): pattern is string => pattern !== undefined);
	if (patterns.length === 0) throw new Error(`${name} has no patterns`);
	return patterns;
}

/** Error texts as pi's Rust port words them: a failed request names its HTTP status, then the response body. */
const STATUS_CASES = [
	`request failed with status 400 Bad Request: {"error":{"message":"Unsupported parameter: 'temperature'.","type":"invalid_request_error"}}`,
	`request failed with status 400 Bad Request: {"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}`,
	`request failed with status 400 Bad Request: {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}`,
	`request failed with status 400 Bad Request: `,
	`request failed with status 401 Unauthorized: {"error":{"message":"Incorrect API key provided.","type":"invalid_request_error","code":"invalid_api_key"}}`,
	`request failed with status 402 Payment Required: {"error":{"message":"Insufficient credits."}}`,
	`request failed with status 403 Forbidden: {"error":{"message":"The model is not available in your region."}}`,
	`request failed with status 404 Not Found: {"error":{"message":"The model does not exist."}}`,
	`request failed with status 408 Request Timeout: `,
	`request failed with status 409 Conflict: {"error":{"message":"The request conflicts with another request."}}`,
	`request failed with status 413 Payload Too Large: {"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}`,
	`request failed with status 413 Payload Too Large: `,
	`request failed with status 422 Unprocessable Entity: {"detail":"messages: field required"}`,
	`request failed with status 429 Too Many Requests: {"error":{"message":"Rate limit reached for requests.","type":"requests"}}`,
	`request failed with status 429 Too Many Requests: {"error":{"message":"You exceeded your current quota.","type":"insufficient_quota","code":"insufficient_quota"}}`,
	`request failed with status 429 Too Many Requests: {"message":"Too many tokens, please wait before trying again."}`,
	`request failed with status 429 Too Many Requests: {"type":"error","error":{"type":"GoUsageLimitError","message":"Monthly usage limit reached"}}`,
	`request failed with status 500 Internal Server Error: {"error":{"message":"The server had an error while processing your request."}}`,
	`request failed with status 502 Bad Gateway: <html><body>bad gateway</body></html>`,
	`request failed with status 503 Service Unavailable: {"error":{"message":"The server is overloaded.","type":"server_error"}}`,
	`request failed with status 503 Service Unavailable: {"message":"Too many tokens, please wait before trying again."}`,
	`request failed with status 504 Gateway Timeout: `,
	`request failed with status 520 <unknown status code>: `,
	`request failed with status 524 <unknown status code>: `,
	`request failed with status 529 <unknown status code>: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`,
];

/** The port's other failures: transport errors, unusable responses, and missing configuration. */
const OTHER_PORT_CASES = [
	"request failed: error sending request for url (https://api.example.test/v1/messages)",
	"request failed: error sending request for url (https://api.example.test/v1/messages) (timed out)",
	"request failed: error decoding response body",
	"provider anthropic requires an API key",
	"provider config error: no credentials are configured for anthropic",
	"provider returned an invalid response: overloaded",
	`provider returned an invalid response: incomplete provider response: {"type":"error","error":{"type":"server_error","message":"The server had an error."}}`,
	`provider returned an invalid response: incomplete provider response: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}`,
	"provider returned an invalid response: {}",
	"invalid header value: failed to parse header value",
];

/** Overflow errors as providers word them, from the list in upstream's overflow.ts, and its exclusions. */
const OVERFLOW_CASES = [
	"prompt is too long: 213462 tokens > 200000 maximum",
	`413 {"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}`,
	"Your input exceeds the context window of this model",
	"Requested token count exceeds the model's maximum context length of 131072 tokens",
	"Input length (265330) exceeds model's maximum context length (262144).",
	"The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
	"This model's maximum prompt length is 131072 but the request contains 537812 tokens",
	"Please reduce the length of the messages or completion",
	"This endpoint's maximum context length is 200000 tokens. However, you requested about 250000 tokens",
	"Input length 300000 exceeds the maximum allowed input length of 262144 tokens.",
	"The input (300000 tokens) is longer than the model's context length (262144 tokens).",
	"the request exceeds the available context size, try increasing it",
	"tokens to keep from the initial prompt is greater than the context length",
	"prompt token count of 150000 exceeds the limit of 128000",
	"invalid params, context window exceeds limit",
	"Your request exceeded model token limit: 262144 (requested: 300000)",
	"Prompt has 9,000 tokens, but the configured context size is 8,192 tokens",
	"Prompt contains 300000 tokens and 0 draft tokens, too large for model with 262144 maximum context length",
	`{"code":"1261","message":"Prompt too long"}`,
	`{"code":"1261","message":"Prompt exceeds max length"}`,
	"Range of input length should be [1, 129024]",
	"prompt too long; exceeded max context length by 100 tokens",
	"Input is too long for requested model.",
	"model_context_window_exceeded",
	"context length exceeded",
	"token limit exceeded",
	"400 status code (no body)",
	"Throttling error: Too many tokens, please wait before trying again.",
	"Service unavailable: too many tokens in flight",
	"ThrottlingException: Too many tokens, please wait before trying again.",
	"rate limit reached: too many tokens per minute",
	"Too many requests: token limit exceeded for this minute",
	"The response was cut off at the maximum context length",
];

/** Transient and permanent failures as providers and transports word them. */
const RETRY_CASES = [
	"overloaded_error",
	"server_busy",
	"Our servers are currently busy, please try again later",
	"We are currently experiencing high demand",
	"The model is at capacity",
	"rate_limit_exceeded",
	"RateLimitError",
	"Too Many Requests",
	"Service Unavailable",
	"service_unavailable",
	"Internal server error",
	"internal_error",
	"Provider returned error",
	"exceeded request buffer limit while retrying upstream",
	"Network error",
	"Connection error.",
	"connect ECONNREFUSED 127.0.0.1:443: connection refused",
	"Connection lost",
	"other side closed",
	"fetch failed",
	"getaddrinfo ENOTFOUND api.example.test",
	"EAI_AGAIN api.example.test",
	"upstream connect error or disconnect/reset before headers",
	"socket hang up",
	"The socket connection was closed unexpectedly",
	"Request timed out.",
	"The operation time out",
	"ETIMEDOUT: timeout",
	"terminated",
	"WebSocket closed with code 1006",
	"websocket error",
	"stream ended without sending any chunks",
	"Anthropic stream ended before message_stop",
	"stream ended before a terminal response event",
	"http2 request did not get a response",
	"The pending stream has been canceled (caused by: )",
	"Server requested 120s retry delay (max: 60s). Rate limited",
	"An error occurred while processing your request. You can retry your request.",
	"Please try your request again in a few seconds",
	"Something went wrong; please retry your request",
	"ResourceExhausted: out of capacity",
	"subscription_sharing_usage_unavailable",
	"subscription_sharing_user_unavailable",
	"GoUsageLimitError: rate limit",
	"FreeUsageLimitError",
	"Monthly usage limit reached (429)",
	"429 Enable usage from your available balance to continue",
	"insufficient_quota",
	"You are out of budget (HTTP 429)",
	"Quota exceeded for this project, 429",
	"Billing hard limit has been reached, 429",
	"subscription_sharing_usage_limit_exceeded",
	"Invalid API key",
	"The model `missing` does not exist",
	"Unknown error",
	"",
];

function errorMessage(message: string): AssistantMessage {
	return {
		role: "assistant",
		content: [],
		api: "openai-completions",
		provider: "stub",
		model: "stub-model",
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "error",
		errorMessage: message,
		timestamp: 0,
	};
}

async function main() {
	const outputDir = process.argv[2];
	if (!outputDir) {
		throw new Error("usage: error-classification-fixture.ts <output-dir>");
	}
	const retrySource = await readFile(RETRY_SOURCE, "utf8");
	const overflowSource = await readFile(OVERFLOW_SOURCE, "utf8");
	const overflow = getOverflowPatterns();
	if (overflow.some((pattern) => pattern.flags !== "i")) {
		throw new Error("an overflow pattern does not ignore case, or has other flags");
	}
	const cases = [...STATUS_CASES, ...OTHER_PORT_CASES, ...OVERFLOW_CASES, ...RETRY_CASES].map((message) => ({
		message,
		retryable: isRetryableAssistantError(errorMessage(message)),
		overflow: isContextOverflow(errorMessage(message)),
	}));
	await mkdir(outputDir, { recursive: true });
	await writeFile(
		join(outputDir, "error-classification.json"),
		`${JSON.stringify(
			{
				source: {
					repository: "https://github.com/earendil-works/pi",
					ref: "main",
					script: fileURLToPath(import.meta.url),
				},
				format: {
					patterns:
						"Regular expression sources, all matched ignoring case. A message is retryable when it matches a retryable pattern and no nonRetryable one; it is a context overflow when it matches an overflow pattern and no nonOverflow one.",
					cases: "For each error message, isRetryableAssistantError and isContextOverflow of an assistant message that failed with it. The provider is not Cerebras, whose bodyless 400 and 413 upstream also reads as overflow.",
				},
				patterns: {
					retryable: stringPatterns(retrySource, "RETRYABLE_PROVIDER_ERROR_PATTERN"),
					nonRetryable: stringPatterns(retrySource, "NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN"),
					overflow: overflow.map((pattern) => pattern.source),
					nonOverflow: regexPatterns(overflowSource, "NON_OVERFLOW_PATTERNS"),
				},
				cases,
			},
			null,
			2,
		)}\n`,
	);
}

main().catch((error) => {
	console.error(error);
	process.exitCode = 1;
});
