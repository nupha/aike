use crossbeam::channel::Sender;
use llama_cpp_ffi::LarkToolChoice;

pub type Token = i32;

#[derive(Debug, Clone)]
pub struct GrammarConfig {
    pub(crate) lark_grammar: String,
    pub(crate) tool_choice: LarkToolChoice,
}

static NEXT_TASK_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

#[derive(Debug, Clone)]
pub struct TaskConfig {
    pub task_id: usize,
    pub grammar: Option<GrammarConfig>,
    pub stop_words: Vec<String>,
    pub max_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub logits_for_all_tokens: bool,
    pub output_mode: OutputMode,
}

impl TaskConfig {
    pub fn new() -> Self {
        Self {
            task_id: NEXT_TASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stop_words: Vec::new(),
            max_tokens: 0,
            temperature: 1.0,
            top_p: 1.0,
            logits_for_all_tokens: false,
            grammar: None,
            output_mode: OutputMode::OneShot,
        }
    }

    pub fn stop_words(mut self, v: Vec<String>) -> Self {
        self.stop_words = v;
        self
    }

    pub fn max_tokens(mut self, v: usize) -> Self {
        self.max_tokens = v;
        self
    }

    pub fn temperature(mut self, v: f32) -> Self {
        self.temperature = v;
        self
    }

    pub fn top_p(mut self, v: f32) -> Self {
        self.top_p = v;
        self
    }

    pub fn logits_for_all_tokens(mut self, v: bool) -> Self {
        self.logits_for_all_tokens = v;
        self
    }

    pub fn grammar(mut self, g: GrammarConfig) -> Self {
        self.grammar = Some(g);
        self
    }

    pub fn output_mode(mut self, v: OutputMode) -> Self {
        self.output_mode = v;
        self
    }
}

pub struct EngineTask {
    pub config: TaskConfig,
    pub prompt_tokens: Vec<Token>,
    pub seq_hint: Option<usize>,
    pub use_cache: bool,
    pub output_tx: Sender<EngineReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Continuous,
    OneShot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCtrlMsg {
    Shutdown,
    CancelTask(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Eog,
    MaxTokens,
    PrefillOnly,
    Word(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineReply {
    Text(String),
    Stop(StopReason, usize),
    Cancelled,
    Error(String),
    Shutdown,
}

#[derive(Debug)]
pub struct TaskState {
    pub config: TaskConfig,
    pub n_completion_tokens: usize,
    pub generated_text: String,
    pub pending_bytes: Vec<u8>,
    pub output_tx: Sender<EngineReply>,
}

impl TaskState {
    pub fn append_utf8_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }

        self.pending_bytes.extend_from_slice(piece);

        loop {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(s) => {
                    self.generated_text.push_str(s);
                    self.pending_bytes.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid == 0 {
                        break;
                    }
                    let s = unsafe { std::str::from_utf8_unchecked(&self.pending_bytes[..valid]) };
                    self.generated_text.push_str(s);
                    self.pending_bytes.drain(0..valid);
                    if self.pending_bytes.is_empty() {
                        break;
                    }
                }
            }
        }
    }
}

pub enum SchedResult {
    Scheduled,
    Deferred(Box<EngineTask>),
    Rejected,
}
