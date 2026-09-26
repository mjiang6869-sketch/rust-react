import type { EngineState } from '../api/types'
import { num, pnlClass, signed } from '../format'

/** 行情页盘口下方的只读账户概况；金额沿用后端的资产口径。 */
export function AccountSnapshot({ engine }: { engine: EngineState }) {
  return (
    <section className="panel market-account" aria-labelledby="market-account-title">
      <div className="panel-head">
        <h2 id="market-account-title">账户概况</h2>
        <span className="head-note">{engine.symbol} · {engine.instrument.settlement_asset}</span>
      </div>
      <div className="market-account-equity">
        <span>账户权益</span>
        <strong>{num(engine.equity, 2)}</strong>
      </div>
      <dl className="market-account-stats">
        <div>
          <dt>未实现盈亏</dt>
          <dd className={pnlClass(engine.unrealized_pnl)}>{signed(engine.unrealized_pnl)}</dd>
        </div>
        <div>
          <dt>已实现盈亏</dt>
          <dd className={pnlClass(engine.realized_pnl)}>{signed(engine.realized_pnl)}</dd>
        </div>
        <div>
          <dt>当前持仓</dt>
          <dd>{engine.position?.side_label ?? '空仓'}</dd>
        </div>
        <div>
          <dt>在途委托</dt>
          <dd>{engine.open_orders.length} 张</dd>
        </div>
        <div>
          <dt>累计手续费</dt>
          <dd>{num(engine.total_fees, 4)}</dd>
        </div>
      </dl>
      {!engine.instrument.fee_is_authoritative && (
        <p className="market-account-note">费率未与交易所账户对账</p>
      )}
    </section>
  )
}
