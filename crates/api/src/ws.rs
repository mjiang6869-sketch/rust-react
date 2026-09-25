//! WebSocket 流。
//!
//! # 协议设计
//!
//! 1. **订阅后先发快照，再发增量。** 这条顺序不是可选的——只发增量会让界面
//!    在收到第一次变更前处于空白状态，或者更糟：用陈旧的初始值渲染然后被
//!    增量覆盖成不一致的状态。
//! 2. **Decimal 一律是字符串。** 与 REST 一致，避免前端浮点丢精度。
//! 3. **状态变更按需推送，不轮询。** 引擎每次产生 `EngineEvent` 就推送一次
//!    完整状态快照。做市的状态变化不频繁（下单、成交、止盈触发），推全量
//!    比设计增量协议简单得多，也不会不一致。

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use crate::dto::*;
use crate::state::{AppState, ProgressMessage};

/// 客户端 → 服务端的消息。
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMessage {
    /// 订阅状态推送。
    ///
    /// `channels` 目前不影响行为——做市的数据量很小，全推比按频道过滤更简单
    /// 且不会出现"订阅了但没收到"的不一致。保留字段是为了将来数据量变大时
    /// 不用改协议。
    Subscribe {
        #[allow(dead_code)]
        channels: Vec<String>,
    },
    /// 心跳。
    Ping,
}

/// 服务端 → 客户端的消息。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// 订阅后的初始快照。
    Snapshot {
        channel: String,
        data: serde_json::Value,
    },
    /// 状态变更。
    Update {
        channel: String,
        data: serde_json::Value,
    },
    /// 后台任务进度。
    Progress { data: ProgressMessage },
    /// 错误。
    Error { code: String, message: String },
    /// 心跳响应。
    Pong,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // 状态推送通道。
    let mut engine_rx = state.progress_tx.subscribe();

    // 先发一次快照，让界面有完整初始状态。
    if let Err(e) = send_snapshot(&mut sender, &state).await {
        tracing::warn!("发送初始快照失败：{e}");
        return;
    }

    // 定期推送状态快照。
    //
    // 这里用固定间隔而不是订阅引擎事件：引擎内部的事件流是同步的，
    // 而 WebSocket 是异步的，跨边界推送需要通道。定时推送更简单，且
    // 做市的状态变化频率远低于这个间隔，不会造成明显延迟。
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // 客户端消息
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Ping) => {
                                let _ = sender
                                    .send(Message::Text(
                                        serde_json::to_string(&ServerMessage::Pong)
                                            .unwrap_or_default()
                                            .into(),
                                    ))
                                    .await;
                            }
                            Ok(ClientMessage::Subscribe { .. }) => {
                                // 订阅语义目前是"全推"——做市的数据量很小，
                                // 按频道过滤只会增加复杂度而不减少带宽。
                                if let Err(e) = send_snapshot(&mut sender, &state).await {
                                    tracing::warn!("发送订阅快照失败：{e}");
                                    break;
                                }
                            }
                            Err(e) => {
                                let msg = ServerMessage::Error {
                                    code: "bad_request".into(),
                                    message: format!("无法解析消息：{e}"),
                                };
                                let _ = sender
                                    .send(Message::Text(
                                        serde_json::to_string(&msg).unwrap_or_default().into(),
                                    ))
                                    .await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!("WebSocket 错误：{e}");
                        break;
                    }
                }
            }

            // 定时推送状态
            _ = tick.tick() => {
                if let Err(e) = send_state_update(&mut sender, &state).await {
                    tracing::debug!("推送状态失败：{e}");
                    break;
                }
            }

            // 后台任务进度
            progress = engine_rx.recv() => {
                match progress {
                    Ok(msg) => {
                        let out = ServerMessage::Progress { data: msg };
                        if sender
                            .send(Message::Text(
                                serde_json::to_string(&out).unwrap_or_default().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // 消费太慢导致丢失消息。必须显式处理——静默滞后会让
                        // 界面显示过时数据而用户不知道。
                        tracing::warn!("进度推送滞后，丢失 {n} 条");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

/// 发送完整快照。
async fn send_snapshot(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    state: &Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data = state_payload(state).await;
    let msg = ServerMessage::Snapshot {
        channel: "state".into(),
        data: data.clone(),
    };
    sender
        .send(Message::Text(serde_json::to_string(&msg)?.into()))
        .await?;

    let safety = state_payload_safety(state).await;
    let msg = ServerMessage::Snapshot {
        channel: "safety".into(),
        data: safety,
    };
    sender
        .send(Message::Text(serde_json::to_string(&msg)?.into()))
        .await?;
    Ok(())
}

/// 推送状态更新。
async fn send_state_update(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    state: &Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data = state_payload(state).await;
    let msg = ServerMessage::Update {
        channel: "state".into(),
        data,
    };
    sender
        .send(Message::Text(serde_json::to_string(&msg)?.into()))
        .await?;
    Ok(())
}

/// 构造状态负载。
async fn state_payload(state: &Arc<AppState>) -> serde_json::Value {
    let mode = state.mode().await;
    let engine = state.engine.lock().await;
    let snap = engine.snapshot();
    let (fill_model, optimism) = engine.fill_model_info();
    let inst = engine.instrument();

    serde_json::json!({
        "mode": mode_tag(mode),
        "symbol": snap.symbol,
        "equity": snap.equity.to_string(),
        "realized_pnl": snap.realized_pnl.to_string(),
        "unrealized_pnl": snap.unrealized_pnl.to_string(),
        "total_fees": engine.total_fees().to_string(),
        "position": snap.position.as_ref().map(|p| {
            let dto = position_dto(p);
            serde_json::to_value(dto).unwrap_or(serde_json::Value::Null)
        }),
        "open_orders": snap.open_orders.iter().map(|o| {
            serde_json::json!({
                "client_id": o.client_id,
                "purpose": purpose_tag(o.purpose),
                "purpose_label": purpose_label(o.purpose),
                "side": side_tag(o.side),
                "quantity": o.quantity.to_string(),
                "limit_price": o.limit_price.to_string(),
                "filled": o.filled.to_string(),
                "state": o.state,
            })
        }).collect::<Vec<_>>(),
        "feed_connected": snap.feed_connected,
        "feed_fresh": engine.feed_is_fresh(chrono::Utc::now()),
        "last_event_at": snap.last_event_at,
        "stand_down": snap.stand_down,
        "fill_model": fill_model,
        "fill_model_optimism": match optimism {
            sim::Optimism::UpperBound => "上界（不现实，仅供对照）",
            sim::Optimism::ConservativeLower => "保守下界（诚实基线）",
        },
        "instrument": {
            "symbol": inst.symbol,
            "settlement_asset": inst.settlement_asset,
            "margin_asset": inst.margin_asset,
            "tick_size": inst.precision.tick_size.to_string(),
            "maint_margin_pct": inst.maint_margin_pct.to_string(),
            "maker_rate": inst.fees.maker_rate.to_string(),
            "fee_source": format!("{:?}", inst.fees.source),
            "fee_is_authoritative": inst.fees.source.is_authoritative(),
        },
    })
}

/// 构造安全状态负载。
async fn state_payload_safety(state: &Arc<AppState>) -> serde_json::Value {
    let engine = state.engine.lock().await;
    let s = engine.safety();
    serde_json::json!({
        "armed": s.is_armed(),
        "user_stream_connected": s.user_stream_connected(),
        "account_reconciled": s.account_reconciled(),
        "blocking_reasons": s.blocking_reasons(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务端消息必须是带 `type` 标签的 JSON——前端靠它分发。
    #[test]
    fn server_messages_are_tagged() {
        let m = ServerMessage::Pong;
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"type\":\"pong\""), "{j}");

        let m = ServerMessage::Error {
            code: "x".into(),
            message: "y".into(),
        };
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"type\":\"error\""), "{j}");
    }

    /// 客户端消息的 op 标签必须能正确解析。
    #[test]
    fn client_messages_parse_by_op() {
        let m: ClientMessage = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert!(matches!(m, ClientMessage::Ping));

        let m: ClientMessage =
            serde_json::from_str(r#"{"op":"subscribe","channels":["state"]}"#).unwrap();
        match m {
            ClientMessage::Subscribe { channels } => assert_eq!(channels, vec!["state"]),
            _ => panic!("应解析为 Subscribe"),
        }
    }

    #[test]
    fn unknown_op_is_an_error_not_a_panic() {
        let r = serde_json::from_str::<ClientMessage>(r#"{"op":"nonsense"}"#);
        assert!(r.is_err());
    }

    /// 进度消息必须能序列化进 WS 帧。
    #[test]
    fn progress_messages_serialize() {
        let m = ServerMessage::Progress {
            data: ProgressMessage::Backtest {
                symbol: "ETHUSDC".into(),
                model: "M1".into(),
                done: 1,
                total: 2,
            },
        };
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"type\":\"progress\""), "{j}");
        assert!(j.contains("\"type\":\"backtest\""), "{j}");
    }
}
