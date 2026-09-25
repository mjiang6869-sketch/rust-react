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

export default function App() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [draft, setDraft] = useState<Config | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let active = true;
    const load = async () => {
      try {
        const result = await request<Snapshot>("/api/state");
        if (!active) return;
        setSnapshot(result);
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

        <section className="metrics" aria-label="运行概况">
          <article className="metric"><span>最新价格</span><strong>{snapshot?.candle ? number(snapshot.candle.close) : "—"}</strong><small>{snapshot?.candle ? time(snapshot.candle.open_time) : "等待行情"}</small></article>
          <article className="metric"><span>可用保证金估值</span><strong>{snapshot ? number(snapshot.available_collateral) : "—"}</strong><small>模拟盘按 USDT/USDC 等值估算</small></article>
          <article className="metric"><span>USDT 余额</span><strong>{snapshot ? number(snapshot.wallet.usdt) : "—"}</strong><small>可在多资产模式下作共享保证金</small></article>
          <article className="metric"><span>USDC 余额</span><strong>{snapshot ? number(snapshot.wallet.usdc) : "—"}</strong><small>USDC 合约盈亏在此结算</small></article>
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
      </main>
    </div>
  );
}
