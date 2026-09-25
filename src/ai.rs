use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

use crate::analysis::{TrendAnalysis, TrendDirection};

#[derive(Clone, Debug, Deserialize)]
pub struct AnalysisRequest {
    pub question: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnalysisResponse {
    pub answer: String,
    pub source: &'static str,
    pub generated_at: DateTime<Utc>,
    pub chart: TrendAnalysis,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

pub async fn analyze(question: String, context: &TrendAnalysis) -> Result<AnalysisResponse> {
    let question = question.trim();
    if question.is_empty() {
        bail!("分析问题不能为空");
    }
    if question.chars().count() > 2_000 {
        bail!("分析问题不能超过 2000 个字符");
    }
    let generated_at = Utc::now();
    let Some(api_key) = std::env::var("RUST_CRYPTO_AI_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(AnalysisResponse {
            answer: deterministic_answer(question, context),
            source: "local-deterministic",
            generated_at,
            chart: context.clone(),
        });
    };
    let base_url = std::env::var("RUST_CRYPTO_AI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
        .trim_end_matches('/')
        .to_string();
    let model = std::env::var("RUST_CRYPTO_AI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let prompt = format!(
        "你是只读行情分析助手。禁止提出或执行下单、撤单、转账和改杠杆操作。\n当前结构化行情上下文：{}\n用户问题：{}\n请用中文回答，明确说明这是研究分析，不保证收益。",
        serde_json::to_string(context).context("序列化行情分析上下文失败")?,
        question
    );
    let body = json!({
        "model": model,
        "temperature": 0.1,
        "messages": [
            { "role": "system", "content": "你只能做只读行情分析，不能调用交易工具。" },
            { "role": "user", "content": prompt }
        ]
    });
    let response: ChatCompletionResponse = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?
        .post(format!("{base_url}/chat/completions"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("AI 服务返回格式无效")?;
    let answer = response
        .choices
        .first()
        .map(|choice| choice.message.content.trim().to_string())
        .filter(|value| !value.is_empty())
        .context("AI 服务没有返回分析内容")?;
    Ok(AnalysisResponse {
        answer,
        source: "openai-compatible",
        generated_at,
        chart: context.clone(),
    })
}

fn deterministic_answer(question: &str, context: &TrendAnalysis) -> String {
    let direction = match context.direction {
        TrendDirection::Up => "上行",
        TrendDirection::Down => "下行",
        TrendDirection::Sideways => "震荡",
        TrendDirection::Unknown => "未知",
    };
    format!(
        "本地只读分析：当前结构方向为{}，识别到{}个拐点、{}笔、{}线段和{}个中枢。问题“{}”需要结合更长周期、成交量和订单簿进一步确认。当前结果仅供研究，不会生成或执行任何订单。",
        direction,
        context.pivots.len(),
        context.chan_strokes.len(),
        context.chan_segments.len(),
        context.chan_centers.len(),
        question
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::model::Candle;
    use rust_decimal::Decimal;

    #[test]
    fn builds_local_read_only_analysis_without_credentials() {
        let candle = Candle {
            open_time: Utc::now(),
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::ONE,
            closed: true,
        };
        let answer = deterministic_answer("现在是什么趋势？", &analyze(&[candle]));
        assert!(answer.contains("不会生成或执行任何订单"));
    }
}
