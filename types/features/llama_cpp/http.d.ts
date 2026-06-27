// Type declarations for the "http" module — gated by feature "llama_cpp"
// These declarations supplement base/http.d.ts via TS declaration merging.

declare module "http" {
    interface HttpServer {
        /** Mount OpenAI-compatible LLM inference routes. */
        openai(engine: any, mountPath: string): this;
    }
}
