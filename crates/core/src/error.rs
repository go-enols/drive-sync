//! 统一错误类型。

/// drive-core 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("编解码错误: {0}")]
    Codec(String),

    #[error("密码学错误: {0}")]
    Crypto(String),

    #[error("消息格式错误: {0}")]
    Invalid(String),

    #[error("签名/校验失败: {0}")]
    Verification(String),

    #[error("协议错误: {0}")]
    Protocol(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
