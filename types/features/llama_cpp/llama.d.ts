// Type declarations for the "llama" module
// import { Model, Client, FileType, KVCacheType, getDevices } from "llama";

declare module "llama" {
  // ---------------------------------------------------------------------------
  // Enums
  // ---------------------------------------------------------------------------

  /** GGUF file quantization types. */
  export const FileType: {
    readonly ALL_F32: number;
    readonly MOSTLY_F16: number;
    readonly MOSTLY_Q4_0: number;
    readonly MOSTLY_Q4_1: number;
    readonly MOSTLY_Q8_0: number;
    readonly MOSTLY_Q5_0: number;
    readonly MOSTLY_Q5_1: number;
    readonly MOSTLY_Q2_K: number;
    readonly MOSTLY_Q3_K_S: number;
    readonly MOSTLY_Q3_K_M: number;
    readonly MOSTLY_Q3_K_L: number;
    readonly MOSTLY_Q4_K_S: number;
    readonly MOSTLY_Q4_K_M: number;
    readonly MOSTLY_Q5_K_S: number;
    readonly MOSTLY_Q5_K_M: number;
    readonly MOSTLY_Q6_K: number;
    readonly MOSTLY_IQ2_XXS: number;
    readonly MOSTLY_IQ2_XS: number;
    readonly MOSTLY_Q2_K_S: number;
    readonly MOSTLY_IQ3_XS: number;
    readonly MOSTLY_IQ3_XXS: number;
    readonly MOSTLY_IQ1_S: number;
    readonly MOSTLY_IQ4_NL: number;
    readonly MOSTLY_IQ3_S: number;
    readonly MOSTLY_IQ3_M: number;
    readonly MOSTLY_IQ2_S: number;
    readonly MOSTLY_IQ2_M: number;
    readonly MOSTLY_IQ4_XS: number;
    readonly MOSTLY_IQ1_M: number;
    readonly MOSTLY_BF16: number;
    readonly MOSTLY_TQ1_0: number;
    readonly MOSTLY_TQ2_0: number;
    readonly MOSTLY_MXFP4_MOE: number;
    readonly MOSTLY_NVFP4: number;
    readonly MOSTLY_Q1_0: number;
    readonly GUESSED: number;
  };

  /** KV cache quantization types. */
  export const KVCacheType: {
    readonly F32: number;
    readonly F16: number;
    readonly BF16: number;
    readonly Q8_0: number;
    readonly Q4_0: number;
    readonly Q4_1: number;
    readonly IQ4_NL: number;
    readonly Q5_0: number;
    readonly Q5_1: number;
  };

  // ---------------------------------------------------------------------------
  // getDevices()
  // ---------------------------------------------------------------------------

  interface DeviceInfo {
    type: string;
    name: string;
  }

  /** List available compute devices (GPUs, CPUs). */
  export function getDevices(): DeviceInfo[];

  // ---------------------------------------------------------------------------
  // Model options / context params
  // ---------------------------------------------------------------------------

  interface ModelOptions {
    gpuLayers?: number | "auto" | "all";
    splitMode?: string | number;
    mainGpu?: number;
    vocabOnly?: boolean;
    useMmap?: boolean;
    useDirectIo?: boolean;
    useMlock?: boolean;
    checkTensors?: boolean;
    useExtraBufts?: boolean;
    noHost?: boolean;
    noAlloc?: boolean;
  }

  interface RopeOptions {
    scalingType?: string;
    freqBase?: number;
    freqScale?: number;
    yarnExtFactor?: number;
    yarnAttnFactor?: number;
    yarnBetaFast?: number;
    yarnBetaSlow?: number;
    yarnOrigContext?: number;
  }

  interface CacheOptions {
    k?: number;
    v?: number;
  }

  interface ContextParams {
    contextSize?: number;
    batchSize?: number;
    ubatchSize?: number;
    sequences?: number;
    threads?: number;
    threadsBatch?: number;
    attentionType?: string;
    flashAttention?: boolean;
    swaFull?: boolean;
    poolingType?: string;
    embeddings?: boolean;
    rope?: RopeOptions;
    cache?: CacheOptions;
    defragThreshold?: number;
    kvUnified?: boolean;
    offloadKqv?: boolean;
    opOffload?: boolean;
    noPerf?: boolean;
  }

  // ---------------------------------------------------------------------------
  // Sampler
  // ---------------------------------------------------------------------------

  interface PenaltyOptions {
    repeatLastN?: number;
    repeatPenalty?: number;
    frequencyPenalty?: number;
    presencePenalty?: number;
  }

  type SamplerStep =
    | {
        kind: "penalties";
        repeatLastN?: number;
        repeatPenalty?: number;
        frequencyPenalty?: number;
        presencePenalty?: number;
      }
    | { kind: "top-k"; k: number }
    | { kind: "top-p"; p: number }
    | { kind: "temperature"; temperature: number };

  /** Sampling chain for token generation. Created via Model.createSamplerChain(). */
  export class SamplerChain {
    readonly model: Model;
    penalties(options?: PenaltyOptions): this;
    topK(k: number): this;
    topP(p: number): this;
    temperature(temperature: number): this;
  }

  // ---------------------------------------------------------------------------
  // Render template options
  // ---------------------------------------------------------------------------

  interface RenderTemplateOptions {
    tools?: any[];
    add_generation_prompt?: boolean;
    enable_thinking?: boolean;
    add_vision_id?: boolean;
    eos_token?: string;
    bos_token?: string;
    [key: string]: any;
  }

  // ---------------------------------------------------------------------------
  // Model
  // ---------------------------------------------------------------------------

  /** A loaded GGUF language model. */
  export class Model {
    constructor(modelPath: string, options?: ModelOptions);
    static load(modelPath: string, options?: ModelOptions): Promise<Model>;

    readonly path: string;
    readonly desc: string;
    readonly parameters: number;
    readonly vocabSize: number;
    readonly bosToken: number;
    readonly bosText: string | null;
    readonly eosToken: number;
    readonly eosText: string | null;
    readonly eotToken: number;
    readonly eotText: string | null;
    readonly sepToken: number;
    readonly sepText: string | null;

    chatTemplate: string | null;

    createContext(params?: ContextParams): Context;
    createSamplerChain(steps: SamplerStep[]): SamplerChain;

    tokenize(text: string): Int32Array;
    detokenize(tokens: Int32Array | number[]): string;
    lookupTokenText(token: number): string | null;
    isEog(token: number): boolean;
    getMeta(key: string): string | null;
    getMetaKeys(): string[];
    renderTemplate(messages: any[], options?: RenderTemplateOptions): string;
    dispose(): void;
  }

  // ---------------------------------------------------------------------------
  // Context
  // ---------------------------------------------------------------------------

  interface ContextStats {
    memoryUsed: number;
    kvCacheSize: number;
    usedTokens: number;
    maxTokens: number;
  }

  interface EngineOptions {
    idleSleepMs?: number;
    slotMatchThreshold?: number;
    maxCompletionTokens?: number;
    cacheReuse?: number;
    cacheRam?: number;
  }

  /** Inference context for a loaded model. Created via Model.createContext(). */
  export class Context {
    readonly contextSize: number;
    readonly batchSize: number;
    readonly isDisposed: boolean;
    readonly model: Model;

    decode(
      tokens: Int32Array | number[],
      startPosition: number,
      logitsOnlyLast?: boolean,
    ): Promise<number | null>;
    sample(logitsIndex: number, sampler: SamplerChain): Promise<number>;
    reset(): Promise<void>;
    embedTokens(tokens: Int32Array | number[]): Promise<Float32Array>;
    getStats(): Promise<ContextStats>;
    createEngine(engineOptions?: EngineOptions): Engine;
    dispose(): void;
  }

  // ---------------------------------------------------------------------------
  // Chat types  —  shared by Engine and Client
  // ---------------------------------------------------------------------------

  export interface ChatMessage {
    role: "system" | "user" | "assistant" | "tool";
    content?: string;
    tool_calls?: any[];
    tool_call_id?: string;
    [key: string]: any;
  }

  interface ResponseFormat {
    type: string;
    json_schema?: any;
  }

  interface StreamOptions {
    include_usage?: boolean;
  }

  /** Token usage statistics. */
  export interface ChatUsage {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  }

  /** Chat completion options (snake_case, matching OpenAI API). */
  export interface ChatOptions {
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
    stream_options?: StreamOptions;
  }

  /** Chat request (camelCase variant, accepted by Client). */
  export interface ChatRequest {
    messages: ChatMessage[];
    model?: string;
    temperature?: number;
    topP?: number;
    maxCompletionTokens?: number;
    stop?: string | string[];
    tools?: any[];
    toolChoice?: string | object;
    parallelToolCalls?: boolean;
    reasoningEffort?: string;
    responseFormat?: ResponseFormat;
    streamOptions?: StreamOptions;
    timeout?: number;
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
  // Engine  —  local LLM inference engine
  // ---------------------------------------------------------------------------

  /** Local LLM inference engine with OpenAI-compatible chat API. Created via Context.createEngine(). */
  export class Engine {
    readonly model: Model;
    parseReasoning: boolean;

    chatOnce(options: ChatOptions): Promise<ChatResponse>;
    chat(options: ChatOptions): AsyncIterableIterator<ChatStreamChunk>;
    responsesOnce(options: ChatOptions): Promise<any>;
    responses(options: ChatOptions): AsyncIterableIterator<any>;
    dispose(): void;
  }

  // ---------------------------------------------------------------------------
  // Client  —  remote LLM client
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

    chatOnce(options: ChatOptions | ChatRequest): Promise<ChatResponse>;
    chat(
      options: ChatOptions | ChatRequest,
    ): AsyncIterableIterator<ChatStreamChunk>;
    responses(options: ChatOptions | ChatRequest): AsyncIterableIterator<any>;
    responsesOnce(options: ChatOptions | ChatRequest): Promise<any>;
    dispose(): void;
  }
}
