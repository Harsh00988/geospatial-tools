use anyhow::Result;
use serde::Serialize;

use crate::path::ConvertPath;

#[derive(Debug, Clone, Serialize)]
pub struct ConvertStats {
    pub input: String,
    pub output: String,
    pub path: String,
    pub seconds: f64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ConvertStats {
    pub fn success(
        input: impl Into<String>,
        output: impl Into<String>,
        path: ConvertPath,
        seconds: f64,
    ) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            path: path.to_string(),
            seconds,
            success: true,
            error: None,
        }
    }

    pub fn failure(
        input: impl Into<String>,
        output: impl Into<String>,
        path: ConvertPath,
        seconds: f64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            path: path.to_string(),
            seconds,
            success: false,
            error: Some(error.into()),
        }
    }
}

pub fn print_json(stats: &ConvertStats) -> Result<()> {
    println!("{}", serde_json::to_string(stats)?);
    Ok(())
}
