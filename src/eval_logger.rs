use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EvalLogEntry {
    /// 请求 ID
    pub request_id: String,
    /// 时间戳
    pub timestamp: String,
    /// 用户输入
    pub input: String,
    /// 模型输出
    pub output: String,
    /// 推理内容（如果有）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 工具调用列表
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallLog>>,
    /// 延迟（毫秒）
    pub latency_ms: u128,
    /// prompt token 数
    pub prompt_tokens: usize,
    /// completion token 数
    pub completion_tokens: usize,
    /// 停止原因
    pub stop_reason: String,
    /// 模型名称
    pub model: String,
    /// 是否流式
    pub is_stream: bool,
    /// 额外上下文（用于 RAG 场景）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval_context: Option<Vec<String>>,
    /// 期望输出（用于测试场景）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCallLog {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 评估日志管理器
pub struct EvalLogger {
    log_dir: PathBuf,
    current_file: Mutex<Option<std::fs::File>>,
}

impl EvalLogger {
    pub fn new(log_dir: &str) -> Self {
        let dir = PathBuf::from(log_dir);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).expect("failed to create eval log dir");
        }

        Self {
            log_dir: dir,
            current_file: Mutex::new(None),
        }
    }

    /// 获取或创建当前日志文件
    fn get_or_create_file(&self) -> std::io::Result<std::fs::File> {
        let mut guard = self.current_file.lock().unwrap();

        if guard.is_none() {
            let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let file_path = self.log_dir.join(format!("eval_{}.jsonl", timestamp));

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)?;

            *guard = Some(file);
        }

        Ok(guard.as_ref().unwrap().try_clone()?)
    }

    /// 记录评估日志
    pub fn log(&self, entry: &EvalLogEntry) -> Result<(), String> {
        let json = serde_json::to_string(entry)
            .map_err(|e| format!("failed to serialize eval log: {}", e))?;

        let mut file = self
            .get_or_create_file()
            .map_err(|e| format!("failed to open log file: {}", e))?;

        writeln!(file, "{}", json).map_err(|e| format!("failed to write eval log: {}", e))?;

        Ok(())
    }
}

/// 全局评估日志器（懒加载单例）
use std::sync::OnceLock;

static EVAL_LOGGER: OnceLock<EvalLogger> = OnceLock::new();

pub fn get_eval_logger() -> &'static EvalLogger {
    EVAL_LOGGER.get_or_init(|| EvalLogger::new("eval_logs"))
}
