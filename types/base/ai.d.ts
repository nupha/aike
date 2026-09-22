declare module "ai" {
  export interface ChatMessage {
    role: "system" | "user" | "assistant" | "tool";
    content?: string;
    reasoning_content?: string;
    tool_calls?: ToolCall[];
    tool_call_id?: string;
  }

  export interface ToolCall {
    id: string;
    function: ToolCallFunction;
  }

  export interface ToolCallFunction {
    name: string;
    arguments: string;
  }

  interface ResponseFormat {
    type: string;
    json_schema?: unknown;
  }

  /** Token usage statistics (OpenAI-compatible). */
  export interface ChatUsage {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
    /** Breakdown of prompt tokens (OpenAI extension). */
    prompt_tokens_details?: {
      /** Number of tokens served from cache. */
      cached_tokens?: number;
      /** Number of audio tokens in the prompt. */
      audio_tokens?: number;
    };
    /** Breakdown of completion tokens (OpenAI extension). */
    completion_tokens_details?: {
      /** Number of reasoning tokens. */
      reasoning_tokens?: number;
      /** Number of audio tokens in the completion. */
      audio_tokens?: number;
      /** Number of accepted prediction tokens. */
      accepted_prediction_tokens?: number;
      /** Number of rejected prediction tokens. */
      rejected_prediction_tokens?: number;
    };
  }

  /** Chat completion request body (snake_case, matching OpenAI API). */
  export interface ChatRequest {
    /** AbortSignal to cancel this request mid-flight. When aborted, `chat`/
     * `responses` iterators reject and `chatOnce`/`responsesOnce` promises
     * reject; the upstream connection is dropped. NOT sent to the API. */
    signal?: AbortSignal;
    /** Per-request HTTP total timeout in seconds. NOT sent to the API. */
    timeout?: number;
    /**
     * Per-request stream-idle timeout in seconds: abort with an error when no
     * SSE data arrives for this long (any received byte resets the timer).
     * Streaming only; NOT sent to the API.
     */
    idle_timeout?: number;

    messages: ChatMessage[];
    model?: string;
    temperature?: number;
    top_p?: number;
    max_completion_tokens?: number;
    stop?: string | string[];
    tools?: any[];
    tool_choice?: string | object;
    parallel_tool_calls?: boolean;
    reasoning_effort?: string;
    response_format?: ResponseFormat;
    stream_options?: { include_usage?: boolean };

    [key: string]: any;
  }

  export interface ChatResponse {
    id?: string;
    model?: string;
    choices: Array<{
      index?: number;
      message: ChatMessage;
      finish_reason: string | null;
    }>;
    usage?: ChatUsage;
    [key: string]: any;
  }

  export interface ChatStreamChunk {
    id: string;
    model: string;
    choices: Array<{
      index: number;
      delta: Partial<ChatMessage>;
      finish_reason: string | null;
    }>;
    usage?: ChatUsage;
    [key: string]: any;
  }

  // ---------------------------------------------------------------------------
  // LlmClient — remote LLM client (always available)
  // ---------------------------------------------------------------------------

  interface LlmClientOptions {
    apiKey?: string;
    baseUrl?: string;
    model?: string;
    maxTokens?: number;
    timeout?: number;
    extraBody?: object;
    /** Custom HTTP headers sent with every request. */
    headers?: Record<string, string>;
  }

  /** Remote LLM client compatible with OpenAI chat API. */
  export class LlmClient {
    constructor(options?: LlmClientOptions);

    readonly model: string;
    readonly baseUrl: string;
    readonly bufferedToolCall: boolean;
    readonly maxTokens: number;
    readonly extraBody: object | null;
    readonly timeout: number;
    readonly headers: Record<string, string>;

    responseNormalizer: ((response: any) => any) | null;

    /** Insert or update a single HTTP header. Pass `null` to remove the entry;
     * `undefined` is a no-op. Affects every subsequent `chat`/`responses` call. */
    setHeader(name: string, value: string | null | undefined): void;
    /** Read a single header value, or `undefined` if not set. */
    getHeader(name: string): string | undefined;

    chat(req: ChatRequest): AsyncIterableIterator<ChatStreamChunk>;
    chatOnce(req: ChatRequest): Promise<ChatResponse>;
    responses(req: ChatRequest): AsyncIterableIterator<any>;
    responsesOnce(req: ChatRequest): Promise<any>;

    dispose(): void;
  }
}
