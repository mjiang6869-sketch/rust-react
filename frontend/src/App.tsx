import { useEffect, useState } from "react";

type Side = "BUY" | "SELL";
type OrderKind = "entry" | "take_profit" | "stop_limit";
type OrderStatus = "open" | "triggered" | "filled" | "canceled";

interface Config {
  symbol: string;
  quote_asset: string;
  margin_asset: string;
  contract_type: string;
  multi_assets_mode: boolean;
  enabled: boolean;
  margin_pct: string;
  leverage: string;
  stop_pct: string;
  take_profit_pct: string;
  maker_fee_pct: string;
  tick_size: string;
  step_size: string;
  min_qty: string;
  min_notional: string;
}

interface Balances {
  usdt: string;
  usdc: string;
}

interface Order {
  id: string;
  kind: OrderKind;
  side: Side;
  status: OrderStatus;
  quantity: string;
  price: string;
  created_at: string;
  updated_at: string;
}

interface Position {
  side: Side;
  quantity: string;
  entry_price: string;
  stop_price: string;
  opened_at: string;
}

interface Snapshot {
  schema_version: number;
  mode: "PAPER" | "LIVE";
  config: Config;
  wallet: Balances;
  available_collateral: string;
  realized_pnl: Balances;
  position: Position | null;
  orders: Order[];
  candle: { close: string; open_time: string } | null;
  feed_connected: boolean;
  feed_fresh: boolean;
  status: string;
}

interface LiveReadiness {
  mode: "PAPER" | "LIVE";
  endpoint: string;
  endpoint_allowed: boolean;
  api_key_configured: boolean;
  api_secret_configured: boolean;
  can_create_runtime: boolean;
  message: string;
}

interface LiveStatus {
  runtime_created: boolean;
  user_stream_connected: boolean;
  account_reconciled: boolean;
  armed: boolean;
  unresolved_order_ids: string[];
  message: string;
}

interface BacktestTrade {
  side: Side;
  entry_time: string;
  entry_price: string;
  exit_time: string;
  exit_price: string;
  quantity: string;
  exit_reason: "TakeProfit" | "StopLoss";
  pnl: string;
  fees: string;
}

interface BacktestReport {
  initial_equity: string;
  final_equity: string;
  trades: BacktestTrade[];
  equity_curve: { time: string; equity: string }[];
  data_gaps: string[];
  pending_expiries: number;
}

interface TrendAnalysis {
  candles: { open_time: string; high: string; low: string; close: string }[];
  pivots: { time: string; price: string; kind: "HIGH" | "LOW" }[];
  trend_lines: {
    kind: "RESISTANCE" | "SUPPORT";
    start: { time: string; price: string };
    end: { time: string; price: string };
  }[];
  chan_strokes: { start: { time: string; price: string }; end: { time: string; price: string } }[];
  chan_segments: { start: { time: string; price: string }; end: { time: string; price: string } }[];
  chan_centers: { start_time: string; end_time: string; low: string; high: string }[];
  direction: "UP" | "DOWN" | "SIDEWAYS" | "UNKNOWN";
  method: string;
}

interface AiReply {
  answer: string;
  source: string;
  generated_at: string;
}

const kindLabel: Record<OrderKind, string> = {
  entry: "回踩开仓",
  take_profit: "Maker 止盈",
  stop_limit: "限价止损",
};

const statusLabel: Record<OrderStatus, string> = {
  open: "挂单中",
  triggered: "已触发，待成交",
  filled: "已成交",
  canceled: "已撤销",
};

const time = (value: string): string => new Date(value).toLocaleString("zh-CN", { hour12: false });
const number = (value: string): string => {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed.toLocaleString("zh-CN", { maximumFractionDigits: 8 }) : value;
};

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init);
  if (!response.ok) throw new Error((await response.text()) || `请求失败：${response.status}`);
  return (await response.json()) as T;
}

function Field({
  label, value, onChange, hint, step = "any",
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  hint?: string;
  step?: string;
}) {
  return (
    <label className="field">
      <span>{label}</span>
      <input type="number" inputMode="decimal" step={step} min="0" value={value}
        onChange={(event) => onChange(event.target.value)} />
      {hint && <small>{hint}</small>}
    </label>
  );
}

function PriceChart({ analysis }: { analysis: TrendAnalysis | null }) {
  if (!analysis?.candles.length) return <p className="empty">等待足够的已收盘 K 线绘制趋势。</p>;
  const width = 920;
  const height = 320;
  const padding = 28;
  const values = analysis.candles.flatMap((candle) => [Number(candle.high), Number(candle.low)]);
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const x = (index: number) => padding + index * ((width - padding * 2) / Math.max(analysis.candles.length - 1, 1));
  const y = (price: number) => height - padding - ((price - min) / span) * (height - padding * 2);
  const indices = new Map(analysis.candles.map((candle, index) => [candle.open_time, index]));
  const closePoints = analysis.candles.map((candle, index) => `${x(index)},${y(Number(candle.close))}`).join(" ");
  const linePoint = (point: { time: string; price: string }) => {
    const index = indices.get(point.time);
    return index === undefined ? null : `${x(index)},${y(Number(point.price))}`;
  };
  const directionLabel = { UP: "上行", DOWN: "下行", SIDEWAYS: "震荡", UNKNOWN: "未知" }[analysis.direction];
  return (
    <>
      <div className="chart-meta"><span>趋势：{directionLabel}</span><span>方法：{analysis.method}</span><span>拐点：{analysis.pivots.length}</span><span>笔：{analysis.chan_strokes.length}</span><span>线段：{analysis.chan_segments.length}</span><span>中枢：{analysis.chan_centers.length}</span></div>
      <div className="chart-wrap">
        <svg className="price-chart" viewBox={`0 0 ${width} ${height}`} role="img" aria-label="K 线收盘价与趋势标注">
          <polyline points={closePoints} fill="none" stroke="#8db6ff" strokeWidth="2" />
          {analysis.chan_centers.map((center) => {
            const start = indices.get(center.start_time);
            const end = indices.get(center.end_time);
            if (start === undefined || end === undefined) return null;
            const left = x(Math.min(start, end));
            const right = x(Math.max(start, end));
            return <rect key={`${center.start_time}-${center.end_time}`} x={left} y={y(Number(center.high))} width={Math.max(right - left, 4)} height={Math.max(y(Number(center.low)) - y(Number(center.high)), 4)} fill="#6d8d9a" fillOpacity=".16" stroke="#8ab4c2" strokeDasharray="4 4" />;
          })}
          {analysis.chan_segments.map((segment, index) => {
            const start = linePoint(segment.start);
            const end = linePoint(segment.end);
            return start && end ? <line key={`segment-${index}`} x1={start.split(",")[0]} y1={start.split(",")[1]} x2={end.split(",")[0]} y2={end.split(",")[1]} stroke="#d6a86e" strokeWidth="2.5" /> : null;
          })}
          {analysis.trend_lines.map((line) => {
            const start = linePoint(line.start);
            const end = linePoint(line.end);
            return start && end ? <line key={`${line.kind}-${line.start.time}`} x1={start.split(",")[0]} y1={start.split(",")[1]} x2={end.split(",")[0]} y2={end.split(",")[1]} stroke={line.kind === "SUPPORT" ? "#70e0b2" : "#ffc179"} strokeWidth="2" strokeDasharray="6 5" /> : null;
          })}
          {analysis.pivots.map((pivot) => {
            const index = indices.get(pivot.time);
            return index === undefined ? null : <circle key={`${pivot.kind}-${pivot.time}`} cx={x(index)} cy={y(Number(pivot.price))} r="4" fill={pivot.kind === "LOW" ? "#70e0b2" : "#ffc179"} />;
          })}
        </svg>
      </div>
    </>
  );
}

export default function App() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [draft, setDraft] = useState<Config | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [backtest, setBacktest] = useState<BacktestReport | null>(null);
  const [backtestBusy, setBacktestBusy] = useState(false);
  const [analysis, setAnalysis] = useState<TrendAnalysis | null>(null);
  const [readiness, setReadiness] = useState<LiveReadiness | null>(null);
  const [liveStatus, setLiveStatus] = useState<LiveStatus | null>(null);
  const [question, setQuestion] = useState("");
  const [conversation, setConversation] = useState<{ role: "user" | "assistant"; text: string }[]>([]);
  const [aiBusy, setAiBusy] = useState(false);

  useEffect(() => {
    let active = true;
    const load = async () => {
      try {
        const [result, trend, liveReadiness, currentLiveStatus] = await Promise.all([
          request<Snapshot>("/api/state"),
          request<TrendAnalysis>("/api/analysis"),
          request<LiveReadiness>("/api/live/readiness"),
          request<LiveStatus>("/api/live/status"),
        ]);
        if (!active) return;
        setSnapshot(result);
        setAnalysis(trend);
        setReadiness(liveReadiness);
        setLiveStatus(currentLiveStatus);
        setDraft((current) => current ?? result.config);
        setError(null);
      } catch (cause) {
        if (active) setError(cause instanceof Error ? cause.message : "无法连接模拟盘服务");
      }
    };
    void load();
    const timer = window.setInterval(() => { void load(); }, 2_000);
    return () => { active = false; window.clearInterval(timer); };
  }, []);

  const liveAction = async (path: string) => {
    setBusy(true);
    try {
      const result = await request<LiveStatus>(path, { method: "POST" });
      setLiveStatus(result);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "LIVE 操作失败");
    } finally {
      setBusy(false);
    }
  };

  const switchMode = async (mode: "PAPER" | "LIVE") => {
    setBusy(true);
    try {
      const result = await request<Snapshot>("/api/mode", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ mode }),
      });
      setSnapshot(result);
      setDraft(result.config);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "切换执行模式失败");
    } finally {
      setBusy(false);
    }
  };

  const update = (field: keyof Config, value: string | boolean) => {
    setDraft((current) => current ? { ...current, [field]: value } : current);
  };

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    try {
      const result = await request<Snapshot>("/api/config", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...draft, enabled: true }),
      });
      setSnapshot(result);
      setDraft(result.config);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "保存设置失败");
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      const result = await request<Snapshot>("/api/kill", { method: "POST" });
      setSnapshot(result);
      setDraft(result.config);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "停用失败");
    } finally {
      setBusy(false);
    }
  };

  const runBacktest = async () => {
    setBacktestBusy(true);
    try {
      const candles = await request<{ open_time: string; open: string; high: string; low: string; close: string; closed: boolean }[]>("/api/history");
      const result = await request<BacktestReport>("/api/backtest", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ candles }),
      });
      setBacktest(result);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "回测失败");
    } finally {
      setBacktestBusy(false);
    }
  };

  const askAi = async () => {
    const prompt = question.trim();
    if (!prompt || aiBusy) return;
    setAiBusy(true);
    setConversation((current) => [...current, { role: "user", text: prompt }]);
    setQuestion("");
    try {
      const reply = await request<AiReply>("/api/ai/analyze", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ question: prompt }),
      });
      setConversation((current) => [...current, { role: "assistant", text: reply.answer }]);
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "AI 分析失败");
    } finally {
      setAiBusy(false);
    }
  };

  const config = snapshot?.config;
  const isTradFi = config?.contract_type === "TRADIFI_PERPETUAL";
  return (
    <div className="page">
      <a className="skip-link" href="#main">跳转到主要内容</a>
      <header className="topbar">
        <div className="brand"><span className="brand-mark">RC</span><span>Rust Crypto <b>模拟做市台</b></span></div>
        <span className="environment">{snapshot?.mode === "LIVE" ? "实盘模式" : "模拟盘模式"} · Maker-only</span>
      </header>
      <main id="main" className="layout">
        <div className="headline">
          <div>
            <p className="eyebrow">USDⓈ-M 永续合约 · 失败突破回踩策略</p>
            <h1>{config?.symbol ?? "正在载入交易对"}</h1>
            <p className="subheading">使用币安公开 1 分钟 K 线。订单、持仓和盈亏在本地模拟。</p>
          </div>
          <div className="headline-actions">
            <span className={`badge ${snapshot?.feed_fresh ? "good" : "alert"}`}>
              {snapshot?.feed_fresh ? "行情正常" : "行情未就绪"}
            </span>
            <span className={`badge ${config?.enabled ? "good" : "muted"}`}>
              {config?.enabled ? "策略已启用" : "策略已关闭"}
            </span>
          </div>
        </div>

        {error && <div className="error" role="alert">{error}</div>}
        <div className="status-line" role="status">{snapshot?.status ?? "正在连接服务…"}</div>

        {readiness && <div className="readiness-line" role="status">LIVE 状态：{readiness.message}</div>}
        <section className="panel live-panel">
          <div className="panel-heading"><h2>执行模式与 LIVE 安全闸门</h2><span>{liveStatus?.message ?? "尚未读取 LIVE 状态"}</span></div>
          <div className="live-controls">
            <button className="secondary" disabled={busy || snapshot?.mode === "PAPER"} onClick={() => { void switchMode("PAPER"); }}>切换 PAPER</button>
            <button className="secondary" disabled={busy || snapshot?.mode === "LIVE"} onClick={() => { void switchMode("LIVE"); }}>切换 LIVE</button>
            <button className="secondary" disabled={busy || snapshot?.mode !== "LIVE" || liveStatus?.runtime_created} onClick={() => { void liveAction("/api/live/connect"); }}>连接并对账</button>
            <button className="primary" disabled={busy || !liveStatus?.runtime_created || liveStatus.armed} onClick={() => { void liveAction("/api/live/arm"); }}>显式 ARM</button>
            <button className="danger" disabled={busy || !liveStatus?.runtime_created || !liveStatus.armed} onClick={() => { void liveAction("/api/live/disarm"); }}>DISARM</button>
            <button className="secondary" disabled={busy || !liveStatus?.runtime_created} onClick={() => { void liveAction("/api/live/close"); }}>关闭 LIVE</button>
          </div>
          <p className="note">LIVE 只有在用户数据流连接、账户对账并显式 ARM 后才允许提交 Maker 限价单；任何未知订单状态都必须先查询。</p>
        </section>

        <section className="metrics" aria-label="运行概况">
          <article className="metric"><span>最新价格</span><strong>{snapshot?.candle ? number(snapshot.candle.close) : "—"}</strong><small>{snapshot?.candle ? time(snapshot.candle.open_time) : "等待行情"}</small></article>
          <article className="metric"><span>可用保证金估值</span><strong>{snapshot ? number(snapshot.available_collateral) : "—"}</strong><small>模拟盘按 USDT/USDC 等值估算</small></article>
          <article className="metric"><span>USDT 余额</span><strong>{snapshot ? number(snapshot.wallet.usdt) : "—"}</strong><small>可在多资产模式下作共享保证金</small></article>
          <article className="metric"><span>USDC 余额</span><strong>{snapshot ? number(snapshot.wallet.usdc) : "—"}</strong><small>USDC 合约盈亏在此结算</small></article>
        </section>

        <section className="panel chart-panel">
          <div className="panel-heading"><h2>K 线与趋势</h2><span>确定性拐点标注</span></div>
          <PriceChart analysis={analysis} />
        </section>

        <section className="panel ai-panel">
          <div className="panel-heading"><h2>行情分析助手</h2><span>只读，不生成订单</span></div>
          <div className="conversation" aria-live="polite">
            {conversation.length ? conversation.map((message, index) => (
              <div className={`message ${message.role}`} key={`${message.role}-${index}`}>
                <span>{message.role === "user" ? "你" : "分析助手"}</span><p>{message.text}</p>
              </div>
            )) : <p className="empty">可以询问当前趋势、拐点、笔线段和中枢。分析结果只读，不会下单。</p>}
          </div>
          <form className="ai-form" onSubmit={(event) => { event.preventDefault(); void askAi(); }}>
            <input aria-label="行情分析问题" value={question} maxLength={2000} placeholder="例如：当前趋势和最近中枢有什么关系？" onChange={(event) => setQuestion(event.target.value)} />
            <button className="secondary" disabled={aiBusy || !question.trim()} type="submit">{aiBusy ? "分析中…" : "提问"}</button>
          </form>
        </section>

        <div className="main-grid">
          <div className="stack">
            <section className="panel">
              <div className="panel-heading"><h2>合约与资金</h2><span>{isTradFi ? "TradFi 永续" : "加密资产永续"}</span></div>
              <div className="facts">
                <div><span>计价资产</span><strong>{config?.quote_asset ?? "—"}</strong></div>
                <div><span>结算资产</span><strong>{config?.margin_asset ?? "—"}</strong></div>
                <div><span>合约类型</span><strong>{config?.contract_type ?? "—"}</strong></div>
                <div><span>本次模拟 Maker 费率</span><strong>{config?.maker_fee_pct ?? "—"}%</strong></div>
              </div>
              <p className="note">多资产模式仅对应全仓。此处余额和盈亏按结算资产分开记录；真实账户的抵押品折算、费率与可用保证金须由账户接口核验。</p>
            </section>

            <section className="panel">
              <div className="panel-heading"><h2>持仓与保护</h2><span>{snapshot?.position ? "持仓中" : "空仓"}</span></div>
              {snapshot?.position ? (
                <div className="position">
                  <div><span>方向</span><strong>{snapshot.position.side === "BUY" ? "做多" : "做空"}</strong></div>
                  <div><span>数量</span><strong>{number(snapshot.position.quantity)}</strong></div>
                  <div><span>开仓价</span><strong>{number(snapshot.position.entry_price)}</strong></div>
                  <div><span>限价止损</span><strong>{number(snapshot.position.stop_price)}</strong></div>
                </div>
              ) : <p className="empty">暂无持仓，等待有效回踩信号。</p>}
              <div className="pnl"><span>已实现盈亏</span><strong>USDT {number(snapshot?.realized_pnl.usdt ?? "0")} · USDC {number(snapshot?.realized_pnl.usdc ?? "0")}</strong></div>
              <p className="note">限价止损触发后仍可能等待成交；行情跳过限价时不会按市价退出。</p>
            </section>
          </div>

          <section className="panel settings">
            <div className="panel-heading"><h2>策略设置</h2><span>修改后手动保存</span></div>
            {draft ? (
              <div className="form">
                <label className="toggle-row">
                  <span><b>多资产保证金模拟</b><small>开启时 USDT 与 USDC 共同计入可用额</small></span>
                  <input type="checkbox" checked={draft.multi_assets_mode}
                    onChange={(event) => update("multi_assets_mode", event.target.checked)} />
                </label>
                <div className="fields">
                  <Field label="仓位比例 %" value={draft.margin_pct} onChange={(v) => update("margin_pct", v)} />
                  <Field label="杠杆倍数" value={draft.leverage} onChange={(v) => update("leverage", v)} step="1" />
                  <Field label="止损距离上限 %" value={draft.stop_pct} onChange={(v) => update("stop_pct", v)} />
                  <Field label="Maker 止盈 %" value={draft.take_profit_pct} onChange={(v) => update("take_profit_pct", v)} />
                  <Field label="Maker 手续费 %" value={draft.maker_fee_pct} onChange={(v) => update("maker_fee_pct", v)}
                    hint="按账户实际费率填写；优惠可能调整" />
                </div>
                <div className="form-actions">
                  <button className="primary" disabled={busy} onClick={() => { void save(); }}>{busy ? "处理中…" : config?.enabled ? "保存并继续运行" : "保存并启用模拟"}</button>
                  <button className="danger" disabled={busy || !config?.enabled} onClick={() => { void stop(); }}>停用并撤开仓单</button>
                </div>
              </div>
            ) : <p className="empty">正在加载配置…</p>}
          </section>
        </div>

        <section className="panel orders">
          <div className="panel-heading"><h2>最近订单</h2><span>最多显示 50 笔</span></div>
          {snapshot?.orders.length ? (
            <div className="table-scroll"><table>
              <thead><tr><th>时间</th><th>类型</th><th>方向</th><th>数量</th><th>价格</th><th>状态</th></tr></thead>
              <tbody>{snapshot.orders.map((order) => <tr key={order.id}>
                <td>{time(order.created_at)}</td><td>{kindLabel[order.kind]}</td>
                <td>{order.side === "BUY" ? "买入" : "卖出"}</td>
                <td>{number(order.quantity)}</td><td>{number(order.price)}</td>
                <td><span className={`order-status ${order.status}`}>{statusLabel[order.status]}</span></td>
              </tr>)}</tbody>
            </table></div>
          ) : <p className="empty">暂无模拟订单。</p>}
        </section>

        <section className="panel backtest-panel">
          <div className="panel-heading">
            <h2>最近行情回测</h2>
            <button className="secondary" disabled={backtestBusy} onClick={() => { void runBacktest(); }}>
              {backtestBusy ? "计算中…" : "运行回测"}
            </button>
          </div>
          {backtest ? (
            <>
              <div className="metrics compact-metrics">
                <article className="metric"><span>初始权益</span><strong>{number(backtest.initial_equity)}</strong></article>
                <article className="metric"><span>最终权益</span><strong>{number(backtest.final_equity)}</strong></article>
                <article className="metric"><span>完成交易</span><strong>{backtest.trades.length}</strong></article>
                <article className="metric"><span>数据缺口</span><strong>{backtest.data_gaps.length}</strong></article>
              </div>
              {backtest.trades.length ? (
                <div className="table-scroll"><table>
                  <thead><tr><th>方向</th><th>开仓</th><th>平仓</th><th>原因</th><th>盈亏</th><th>手续费</th></tr></thead>
                  <tbody>{backtest.trades.map((trade, index) => (
                    <tr key={`${trade.entry_time}-${index}`}>
                      <td>{trade.side === "BUY" ? "做多" : "做空"}</td>
                      <td>{number(trade.entry_price)}</td>
                      <td>{number(trade.exit_price)}</td>
                      <td>{trade.exit_reason === "TakeProfit" ? "Maker 止盈" : "限价止损"}</td>
                      <td>{number(trade.pnl)}</td>
                      <td>{number(trade.fees)}</td>
                    </tr>
                  ))}</tbody>
                </table></div>
              ) : <p className="empty">当前历史窗口没有完成交易。</p>}
            </>
          ) : <p className="empty">运行回测后显示最近 120 根 K 线的结果。</p>}
        </section>
      </main>
    </div>
  );
}
