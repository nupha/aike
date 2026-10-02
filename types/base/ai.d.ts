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

  /**
   * Chat completion request body — pure JSON-serializable data submitted to
   * the API (snake_case, matching OpenAI API). Control fields (signal,
   * timeouts) belong to `ChatOpts`, the second call argument.
   */
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

    [key: string]: any;
  }

  /**
   * Responses API request body — pure JSON-serializable data. `input` is a
   * plain string or an array of input items (no `messages` field).
   */
  export interface ResponsesRequest {
    input: string | object[];
    model?: string;
    instructions?: string;
    temperature?: number;
    top_p?: number;
    max_output_tokens?: number;
    tools?: any[];
    tool_choice?: string | object;
    metadata?: object;
    user?: string;

    [key: string]: any;
  }

  /**
   * Per-call control options — never serialized into the request body.
   */
  export interface ChatOpts {
    /** AbortSignal to cancel this call mid-flight. When aborted, `chat`/
     * `responses` iterators reject and `chatOnce`/`responsesOnce` promises
     * reject; the upstream connection is dropped. */
    signal?: AbortSignal;
    /** Per-request HTTP total timeout in seconds. */
    timeout?: number;
    /**
     * Per-request stream-stall timeout in seconds: abort with an error when no
     * stream data arrives for this long (the stream stalled; any received byte
     * resets the timer). Streaming only.
     */
    stallTimeout?: number;
    /**
     * Coalescing window in milliseconds for streaming: consecutive
     * pure-content chunks arriving within this window are merged into one
     * chunk before being yielded, reducing per-chunk overhead. 0 (or negative)
     * disables coalescing; defaults to 0. Non-integer values throw
     * RangeError. Changes yielded chunk granularity. Streaming only.
     */
    coalesce?: number;
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
  // System1Client — TypeSafe System One Models (Jev): typed, probabilistic
  // decisions instead of generated text. Mirrors `@typesafe-ai/sdk`.
  // ---------------------------------------------------------------------------

  /** Text, a JSON object, an array, or `null`. */
  export type System1Entry = string | object | unknown[] | null;

  /** Yes/no question with optional descriptions of either outcome. */
  export interface NoulQuestion {
    type: "noul";
    instructions?: System1Entry;
    criteria?: { true?: System1Entry; false?: System1Entry } | null;
  }

  /** One-of-N question; `criteria` maps a label to its description. */
  export interface ChoiceQuestion {
    type: "choice";
    instructions?: System1Entry;
    criteria: Record<string, System1Entry>;
  }

  /**
   * Ordered-rubric question; `criteria` holds the level descriptions, indexed
   * from zero. At least two levels are required. The documented upper bound is
   * 10 levels, but it is not enforced client-side — the server rejects it.
   */
  export interface ScoreQuestion {
    type: "score";
    instructions?: System1Entry;
    criteria: System1Entry[];
  }

  export type System1Question = NoulQuestion | ChoiceQuestion | ScoreQuestion;

  /** `state` plus the questions answered in one parallel pass. */
  export interface SystemOneRequest {
    state: System1Entry;
    questions: Record<string, System1Question>;
    /** Overrides the client's model for this call. */
    model?: string;
    [key: string]: any;
  }

  export interface System1Usage {
    /** Number of input tokens used. */
    readonly input_tokens: number;
    /** Number of output tokens used. Reported even on the free tier. */
    readonly output_tokens: number;
  }

  export interface NoulResponse {
    readonly type: "noul";
    /** Probability of a yes answer, 0 to 1. No confidence field: the number is the belief. */
    readonly noul: number;
  }

  export interface ChoiceResponse {
    readonly type: "choice";
    /** One of the labels declared in the question's `criteria`. */
    readonly choice: string;
    readonly confidence: number;
    /** Probabilities keyed by label; sums to ~1. */
    readonly probabilities: Record<string, number>;
  }

  export interface ScoreResponse {
    readonly type: "score";
    /** Expected score in `[0, levels-1]`; may fall between levels (e.g. 1.035). */
    readonly score: number;
    readonly confidence: number;
    /** Rubric descriptions keyed by level index (`"0"`, `"1"`, …). */
    readonly legend: Record<string, System1Entry>;
    /** Probabilities keyed by level index; sums to ~1. */
    readonly probabilities: Record<string, number>;
  }

  export type System1Answer = NoulResponse | ChoiceResponse | ScoreResponse;

  export interface SystemOneResult {
    /** The versioned model id that answered. */
    readonly model: string;
    readonly answers: Record<string, System1Answer>;
    readonly usage: System1Usage;
  }

  /**
   * Per-call control options — never serialized into the request body. There is
   * no streaming, so the stream-only knobs of `ChatOpts` do not apply.
   */
  export interface SystemOneOpts {
    /** AbortSignal cancelling the in-flight request. */
    signal?: AbortSignal;
    /** Per-request HTTP timeout in seconds. */
    timeout?: number;
  }

  interface System1ClientOptions {
    /** API root. `{baseUrl}/v1/systemone` is the request URL. Required. */
    baseUrl: string;
    /**
     * Model id to answer with, e.g. `jev-1.13`. Required — pin it, since a
     * versionless alias can move and change every answer.
     */
    model: string;
    /** Sent as `Authorization: Bearer <apiKey>`. Omit for a keyless gateway. */
    apiKey?: string;
    /** HTTP timeout in seconds. Default: 60. */
    timeout?: number;
    /** Custom HTTP headers sent with every request. */
    headers?: Record<string, string>;
  }

  /**
   * Client for System One Models (Jev).
   *
   * Answers typed questions about a `state` with probabilities instead of
   * generating text. Reads no environment variables: `baseUrl` and `model`
   * must be given explicitly.
   */
  export class System1Client {
    constructor(options: System1ClientOptions);

    readonly model: string;
    readonly baseUrl: string;
    readonly timeout: number;
    readonly headers: Record<string, string>;

    /** Insert or update a single HTTP header. `null` removes it, `undefined` is a no-op. */
    setHeader(name: string, value: string | null | undefined): void;
    /** Read a single header value, or `undefined` if not set. */
    getHeader(name: string): string | undefined;

    /** Answer every question about `state` in one pass. */
    systemOne(req: SystemOneRequest, opts?: SystemOneOpts): Promise<SystemOneResult>;

    dispose(): void;
  }

  /** Build a one-of-N question. `criteria` maps a label to its description. */
  export function choice(
    instructions: System1Entry,
    criteria: Record<string, System1Entry>,
  ): ChoiceQuestion;
  /** Build an ordered-rubric question; `criteria` needs at least two levels. */
  export function score(
    instructions: System1Entry,
    criteria: System1Entry[],
  ): ScoreQuestion;
  /** Build a yes/no question answered with a probability. */
  export function noul(
    instructions?: System1Entry,
    criteria?: { true?: System1Entry; false?: System1Entry } | null,
  ): NoulQuestion;

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

    chat(
      req: ChatRequest,
      opts?: ChatOpts,
    ): AsyncIterableIterator<ChatStreamChunk>;
    chatOnce(req: ChatRequest, opts?: ChatOpts): Promise<ChatResponse>;
    responses(
      req: ResponsesRequest,
      opts?: ChatOpts,
    ): AsyncIterableIterator<any>;
    responsesOnce(req: ResponsesRequest, opts?: ChatOpts): Promise<any>;

    dispose(): void;
  }
}
