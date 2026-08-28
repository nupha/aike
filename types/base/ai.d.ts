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

  /** Token usage statistics. */
  export interface ChatUsage {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  }

  /** Chat completion request body (snake_case, matching OpenAI API). */
  export interface ChatRequest {
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
    /** Per-request HTTP total timeout in seconds. NOT sent to the API. */
    timeout?: number;
    /**
     * Per-request stream-idle timeout in seconds: abort with an error when no
     * SSE data arrives for this long (any received byte resets the timer).
     * Streaming only; NOT sent to the API.
     */
    idle_timeout?: number;
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
  // Client — remote LLM client (always available)
  // ---------------------------------------------------------------------------

  interface ClientOptions {
    apiKey?: string;
    baseUrl?: string;
    model?: string;
    maxTokens?: number;
    timeout?: number;
    extraBody?: object;
  }

  /** Remote LLM client compatible with OpenAI chat API. */
  export class Client {
    constructor(options?: ClientOptions);

    readonly model: string;
    readonly baseUrl: string;
    readonly bufferedToolCall: boolean;
    readonly maxTokens: number;
    readonly extraBody: object | null;
    readonly timeout: number;

    responseNormalizer: ((response: any) => any) | null;

    chatOnce(options: ChatRequest): Promise<ChatResponse>;
    chat(options: ChatRequest): AsyncIterableIterator<ChatStreamChunk>;
    responses(options: ChatRequest): AsyncIterableIterator<any>;
    responsesOnce(options: ChatRequest): Promise<any>;
    dispose(): void;
  }
}
