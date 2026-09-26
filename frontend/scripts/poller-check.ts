// 轮询状态机与行情推送连接的离线检查。
//
// 直接跑：
//     cd frontend && node --experimental-strip-types scripts/poller-check.ts
//
// # 为什么是脚本而不是单元测试框架
//
// 这个仓库的前端没有测试框架（`package.json` 里只有 dev/build/typecheck），
// 而事故的复现**不需要**渲染 React——它只需要一段能数清"同时有几条定时器链"
// 的逻辑。所以用一个假时钟驱动真正的 [`createPoller`]（不是复制的实现），
// 直接断言观察到的行为。等前端引入测试框架时，这个脚本可以逐条搬进测试。
//
// # 它复现了什么
//
// 用户当初的复现办法是：切走再切回页面，看定时器链从 1 条变成 2 条。真正
// 造成权重翻倍的量是**并发请求数**（每条链各发一发，且都按 1 秒间隔重发），
// 所以这里断言两件事：链数 ≤ 1，并发在途请求峰值 = 1。
//
// 反证一条：把旧写法搬进来，断言它确实会退化。没有这一条，上面那些"新
// 实现没坏"的断言就无法证明测试抓得住这个 bug——永远通过的断言等于没有
// 断言。
//
// 盘口与成交流后来改走推送（`marketSocket.ts`）。推送连接有同样的"多一条"
// 风险——重复 start、旧连接迟到的 close 各自触发一次重连——所以也用假时钟
// 驱动，断言任何时刻最多一条连接。

// `@types/node` 没有装（前端只需要浏览器的类型），而这是一个用 Node 跑的
// 离线脚本。与其为它引入一个只在开发时用得到的类型包、或让整个前端项目
// 的类型环境多一套全局声明，不如在这里声明用到的少量全局与两个断言函数。
// 范围明确，且 `src/` 的浏览器类型环境完全不受影响。

declare const process: { exitCode?: number }
declare const console: { log(...args: unknown[]): void }

import {
  INITIAL_BACKOFF_MS,
  MAX_BACKOFF_MS,
  createMarketSocket,
  type Connection,
  type ConnectionHandlers,
} from '../src/api/marketSocket.ts'
import { createPoller, type Clock, type PollOptions } from '../src/api/poller.ts'

/** 断言为真，否则抛错（由运行器捕获并记为失败）。 */
function assert(cond: unknown, message = '断言失败'): asserts cond {
  if (!cond) throw new Error(message)
}

/** 断言相等。 */
function assertEqual<T>(actual: T, expected: T, message = ''): void {
  if (actual !== expected) {
    throw new Error(`${message}（期望 ${String(expected)}，实际 ${String(actual)}）`)
  }
}

/** 假时钟：不真的等，也能精确控制"冷却还剩多久"。 */
class FakeClock implements Clock {
  private t = 0
  private seq = 0
  private timers = new Map<number, { at: number; fn: () => void }>()
  /** 冷却截止时刻。 */
  private cooldownUntil = 0

  now(): number {
    return this.t
  }

  cooldownRemainingMs(): number {
    return Math.max(0, this.cooldownUntil - this.t)
  }

  /** 施加冷却（模拟收到 429 后前端记下 Retry-After）。 */
  arm(ms: number): void {
    this.cooldownUntil = Math.max(this.cooldownUntil, this.t + ms)
  }

  setTimeout(fn: () => void, ms: number): ReturnType<typeof setTimeout> {
    const id = ++this.seq
    this.timers.set(id, { at: this.t + ms, fn })
    return id as unknown as ReturnType<typeof setTimeout>
  }

  clearTimeout(handle: ReturnType<typeof setTimeout>): void {
    this.timers.delete(handle as unknown as number)
  }

  /** 当前未触发的定时器条数。**这就是"几条链"**。 */
  get pending(): number {
    return this.timers.size
  }

  /**
   * 推进时间。到点的定时器按到期顺序触发，触发过程中新排的定时器
   * 若也在本次窗口内会继续触发——与真实事件循环一致。
   */
  async advance(ms: number, step = 1): Promise<void> {
    const target = this.t + ms
    while (this.t < target) {
      this.t = Math.min(this.t + step, target)
      const due = [...this.timers.entries()]
        .filter(([, v]) => v.at <= this.t)
        .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])
      for (const [id, v] of due) {
        this.timers.delete(id)
        v.fn()
      }
      // 让 await 链推进
      await Promise.resolve()
      await Promise.resolve()
      await Promise.resolve()
    }
  }
}

/** 可控制完成时机的假抓取。 */
function fakeFeed<T>(value: T) {
  const state = {
    /** 发起的请求数。**用来量请求量**。 */
    calls: 0,
    /** 当前未完成的请求数。**用来验"没有重叠"**。 */
    inflight: 0,
    /** 同时在途的最大值。必须 ≤ 1。 */
    maxInflight: 0,
    /** 手动放行：每次 `fetch` 都把 resolver 推到这里。 */
    resolvers: [] as Array<() => void>,
    /** 是否立即完成（用于不需要精细控制的场景）。 */
    auto: true,
    error: null as unknown,
  }

  const fetch = (): Promise<T> => {
    state.calls += 1
    state.inflight += 1
    state.maxInflight = Math.max(state.maxInflight, state.inflight)
    if (state.auto) {
      state.inflight -= 1
      if (state.error !== null) return Promise.reject(state.error)
      return Promise.resolve(value)
    }
    return new Promise<T>((resolve, reject) => {
      state.resolvers.push(() => {
        state.inflight -= 1
        if (state.error !== null) reject(state.error)
        else resolve(value)
      })
    })
  }

  /** 放行所有在途请求。 */
  const release = () => {
    const rs = state.resolvers.splice(0)
    for (const r of rs) r()
  }

  return { fetch, state, release }
}

function harness<T>(opts: {
  clock: FakeClock
  feed: ReturnType<typeof fakeFeed<T>>
  paused?: () => boolean
  intervalMs?: number
  onData?: (d: T) => void
  onError?: (m: string, limited: boolean) => void
  onCooldown?: (ms: number) => void
}) {
  const errors: string[] = []
  const cooldowns: number[] = []
  const data: T[] = []
  const p: PollOptions<T> = {
    intervalMs: opts.intervalMs ?? 1_000,
    clock: opts.clock,
    fetch: opts.feed.fetch,
    isPaused: opts.paused ?? (() => false),
    describe: (e) => (e instanceof Error ? e.message : String(e)),
    isLimited: (e) =>
      typeof e === 'object' && e !== null && 'status' in e &&
      (e as { status: number }).status === 429,
    onData: (d) => {
      data.push(d)
      opts.onData?.(d)
    },
    onError: (m, limited) => {
      errors.push(m)
      opts.onError?.(m, limited)
    },
    onCooldown: (ms) => {
      cooldowns.push(ms)
      opts.onCooldown?.(ms)
    },
  }
  return { poller: createPoller<T>(p), errors, cooldowns, data }
}

const checks: Array<[string, () => Promise<void>]> = []
function check(name: string, fn: () => Promise<void>): void {
  checks.push([name, fn])
}

// ───────────────────────────────────────────────────────────────────────────
// 1. 事故核心：反复切走切回不能把定时器链变成 2 条
// ───────────────────────────────────────────────────────────────────────────

check('反复可见性切换后，定时器链始终 ≤ 1', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  // 请求先不完成，制造"在途时收到 visibilitychange"这一关键时序
  feed.state.auto = false
  let hidden = false
  const { poller } = harness({ clock, feed, paused: () => hidden })

  poller.start()
  assertEqual(clock.pending, 0, '首轮在途，不应有定时器')
  assertEqual(feed.state.calls, 1, '首轮应发 1 次请求')

  // 切走 5 次、切回 5 次——每次都在**请求仍挂在半空时**发生。
  // 这正是旧实现把链数从 1 变成 2 的时序：clearTimeout 清的是一个
  // 早已触发的句柄，清不掉任何东西。
  for (let i = 0; i < 5; i++) {
    hidden = true
    poller.onVisibilityChange()
    await clock.advance(10)
    hidden = false
    poller.onVisibilityChange()
    await clock.advance(10)
    assertEqual(
      clock.pending,
      0,
      `第 ${i + 1} 次切回后不应凭空多出定时器（在途请求还没结束）`,
    )
  }

  // 全程只应该有过 1 次请求：在途期间任何"立刻刷新"都只能记意图
  assertEqual(feed.state.calls, 1, '在途期间的切回不得并发再发请求')
  assertEqual(feed.state.maxInflight, 1, '任何时刻最多 1 个在途请求')

  // 放行后在途请求：收尾时消费 pending，并**只排一条**链
  feed.release()
  await clock.advance(1)
  assertEqual(clock.pending, 1, '收尾后应恰好排 1 条链')

  // 继续跑 5 轮，链数不能累积
  feed.state.auto = true
  await clock.advance(5_500)
  assertEqual(clock.pending <= 1, true, `链数累积了：${clock.pending}`)
  assertEqual(feed.state.maxInflight, 1, '任何时刻最多 1 个在途请求')
})

check('pending 被消费：切回后应立刻补一次，而不是干等一个周期', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  feed.state.auto = false
  let hidden = false
  const { poller } = harness({ clock, feed, paused: () => hidden })

  poller.start()
  assertEqual(feed.state.calls, 1)

  // 请求还在途时切走切回
  hidden = true
  poller.onVisibilityChange()
  hidden = false
  poller.onVisibilityChange()

  // 放行第一发
  feed.release()
  await clock.advance(1)

  // pending 应让下一轮延迟为 0，于是推进极小时间就该发出第二发
  await clock.advance(2)
  assertEqual(feed.state.calls, 2, '切回后应立刻补一次，而不是等满一个周期')
})

// ───────────────────────────────────────────────────────────────────────────
// 2. 不可见时不应留下空转的定时器链
// ───────────────────────────────────────────────────────────────────────────

check('页面不可见时停止排程，不空转', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  let hidden = false
  const { poller } = harness({ clock, feed, paused: () => hidden })

  poller.start()
  await clock.advance(3_100)
  const before = feed.state.calls
  assert(before >= 3, `正常运行应发了多次，实际 ${before}`)

  hidden = true
  poller.onVisibilityChange()
  assertEqual(clock.pending, 0, '切走后不应留有定时器')

  await clock.advance(60_000)
  assertEqual(
    feed.state.calls,
    before,
    '不可见期间**一次请求都不该发**（旧实现是每秒空转）',
  )
  assertEqual(clock.pending, 0, '不可见期间不应累积定时器')

  // 切回来应恢复
  hidden = false
  poller.onVisibilityChange()
  await clock.advance(1)
  assertEqual(feed.state.calls, before + 1, '切回后应立即恢复轮询')
})

// ───────────────────────────────────────────────────────────────────────────
// 3. 限流退避：冷却期内一次都不发
// ───────────────────────────────────────────────────────────────────────────

check('冷却期内不发请求，并按剩余冷却安排下一次', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  const { poller, cooldowns } = harness({ clock, feed })

  poller.start()
  await clock.advance(2_100)
  const before = feed.state.calls

  // 模拟收到 429：前端记下 Retry-After（这里 60 秒）
  clock.arm(60_000)

  await clock.advance(30_000)
  assertEqual(
    feed.state.calls,
    before,
    `冷却期内不应有任何请求，实际多发 ${feed.state.calls - before} 次`,
  )
  assert(cooldowns.length > 0, '冷却中应通知界面（否则用户只看到"请求失败"）')
  assertEqual(clock.pending <= 1, true, '冷却期间链数仍需 ≤ 1')

  // 冷却结束后应恢复
  await clock.advance(35_000)
  assert(
    feed.state.calls > before,
    '冷却结束后必须自动恢复——否则界面就永久停在那了',
  )
})

check('一个不重叠的长请求不会让请求量翻倍', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  // 请求耗时 6 秒，而间隔是 1 秒：`setInterval` 会在这里堆叠
  feed.state.auto = false
  const { poller } = harness({ clock, feed, intervalMs: 1_000 })

  poller.start()
  assertEqual(feed.state.calls, 1)

  // 推进 6 秒：期间定时器到点，但请求还在途
  for (let i = 0; i < 6; i++) {
    await clock.advance(1_000)
  }
  assertEqual(
    feed.state.calls,
    1,
    `长请求期间不应并发重发（setInterval 会发 7 次），实际 ${feed.state.calls}`,
  )
  assertEqual(clock.pending <= 1, true)
})

// ───────────────────────────────────────────────────────────────────────────
// 4. 限流是"退避"而不是"重试"
// ───────────────────────────────────────────────────────────────────────────

check('429 之后按冷却退避，而不是按原间隔硬重试', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  // 这一发返回 429。真实的 `request()` 会在收到 429 时读 `Retry-After` 并
  // 施加冷却——所以这里在 catch 里做同样的事，模拟那条链路。
  const limited = Object.assign(new Error('上游限流'), { status: 429 })
  const { poller, errors } = harness({
    clock,
    feed,
    onError: (_m, wasLimited) => {
      if (wasLimited) clock.arm(30_000)
    },
  })

  poller.start()
  await clock.advance(1_100)

  feed.state.error = limited
  const before = feed.state.calls

  // 推进到下一发：它会失败，并在 `onError(limited=true)` 里施加 30 秒冷却
  await clock.advance(1_200)
  assert(feed.state.calls > before, '这一发应该真的发出去了')
  const afterLimit = feed.state.calls
  assert(errors.length > 0, '限流要显示给用户')

  // 关键：接下来 30 秒一次都不能发。旧实现会按 1 秒间隔继续硬打，
  // 那正是 429 升级成 418 的路径。
  await clock.advance(29_000)
  assertEqual(
    feed.state.calls,
    afterLimit,
    `限流后仍发了 ${feed.state.calls - afterLimit} 次——按原间隔硬重试了`,
  )
  assertEqual(clock.pending <= 1, true)

  // 冷却结束后恢复，且错误应被清除
  feed.state.error = null
  await clock.advance(2_000)
  assert(feed.state.calls > afterLimit, '冷却结束后必须自动恢复')
})

// ───────────────────────────────────────────────────────────────────────────
// 5. 停止后不得再发（卸载后的泄漏）
// ───────────────────────────────────────────────────────────────────────────

check('stop 之后不再发请求，也不留定时器', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  const { poller } = harness({ clock, feed })

  poller.start()
  await clock.advance(3_100)
  assert(feed.state.calls >= 3)

  poller.stop()
  const after = feed.state.calls
  assertEqual(clock.pending, 0, 'stop 后不应留下定时器')
  await clock.advance(60_000)
  assertEqual(feed.state.calls, after, 'stop 之后仍在发请求——组件已卸载')
})

check('在途请求返回时若已 stop，不得再排程', async () => {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  feed.state.auto = false
  const { poller } = harness({ clock, feed })

  poller.start()
  assertEqual(feed.state.calls, 1)
  poller.stop()
  feed.release()
  await clock.advance(100)
  assertEqual(clock.pending, 0, '卸载后迟到的响应把轮询又拉起来了')
  assertEqual(feed.state.calls, 1)
})

// ───────────────────────────────────────────────────────────────────────────
// 6. 反证：旧写法必须能复现重复轮询
// ───────────────────────────────────────────────────────────────────────────

/**
 * 用**旧写法**（`clearTimeout` + 直接再调一次）跑同一组时序，断言它确实
 * 会退化成 2 条链。
 *
 * 这一条存在的意义：其余检查都是"新实现没坏"，而它们**不能证明**测试本身
 * 抓得住这个 bug——一个永远通过的断言等于没有断言。这里把旧实现搬进来
 * 当反例，如果哪天它不再复现，说明测试的时序变了、其余断言也就失去了
 * 意义，这一条会先失败提醒。
 *
 * 用户当初的复现办法正是这个：切走再切回，看定时器数从 1 变成 2。
 */
check('反证：旧写法的重复轮询必须被测出来（否则上面的断言不可信）', async () => {
  const timers = new Set<number>()
  let seq = 0
  let timer: number | null = null
  let hidden = false
  let calls = 0
  let inflight = 0
  let maxInflight = 0
  const resolvers: Array<() => void> = []

  const schedule = (): void => {
    const id = ++seq
    timers.add(id)
    // 关键：句柄记到 `timer`，但**触发后不清零**——这正是旧实现的写法。
    // 于是 `timer !== null` 恒为真，而它指向的定时器早已触发过，
    // `clearTimeout` 清不掉任何还在等待的东西。
    timer = id
    void Promise.resolve().then(() => {
      if (!timers.has(id)) return // 已被 clearTimeout 取消
      timers.delete(id) // 定时器到期，自动出队
      void tick()
    })
  }

  const tick = async (): Promise<void> => {
    if (hidden) {
      schedule()
      return
    }
    inflight += 1
    calls += 1
    maxInflight = Math.max(maxInflight, inflight)
    await new Promise<void>((r) => resolvers.push(r))
    inflight -= 1
    schedule()
  }

  // 旧实现的可见性处理：清掉一个**可能早已触发的**句柄，然后直接再调一次
  const onVisible = (): void => {
    if (hidden) return
    if (timer !== null) {
      timers.delete(timer)
      timer = null
    }
    void tick()
  }

  schedule()
  await Promise.resolve()
  assertEqual(calls, 1, '首轮应已发起')

  // 请求挂在半空时反复切走切回
  for (let i = 0; i < 5; i++) {
    hidden = true
    onVisible()
    hidden = false
    onVisible()
    await Promise.resolve()
  }

  // 断言的是**并发请求数**而不是"定时器条数"：定时器在触发后都会出队，
  // 真正的负载倍增来自"同一时刻有多个请求在飞"。这才是权重翻倍的原因，
  // 也正是新实现用 `inflight` 挡住的东西。
  assert(
    calls > 1,
    '旧写法居然没并发重发——测试时序已失去复现能力，其余断言不再可信',
  )
  assert(
    maxInflight > 1,
    `旧写法应出现并发请求，实际并发峰值 ${maxInflight}——测试未能复现事故`,
  )

  console.log(
    `      （旧写法实测：5 次切换 → 请求 ${calls} 次、并发峰值 ${maxInflight}；` +
      `新实现同期为 1 次、峰值 1）`,
  )
})

// ───────────────────────────────────────────────────────────────────────────
// 7. 行情推送连接：任何时刻最多一条
// ───────────────────────────────────────────────────────────────────────────

/** 假推送服务：记录建过几条连接，并能从外面触发开、收、断。 */
function fakeSockets() {
  const conns: Array<{ h: ConnectionHandlers; closed: boolean; sent: string[] }> = []
  const open = (_url: string, h: ConnectionHandlers): Connection => {
    const c = { h, closed: false, sent: [] as string[] }
    conns.push(c)
    return {
      send: (t) => c.sent.push(t),
      close: () => {
        c.closed = true
      },
    }
  }
  /** 当前没被关掉的连接数——**这就是"几条推送连接"**。 */
  const alive = () => conns.filter((c) => !c.closed).length
  /** 服务端断开最新的那条连接（网络断、后端重启）。 */
  const drop = () => {
    const c = conns.at(-1)
    if (c === undefined) return
    c.closed = true
    c.h.onClose()
  }
  return { conns, open, alive, drop }
}

const FRAME = (symbol: string) =>
  JSON.stringify({
    type: 'market',
    symbol,
    source: 'x',
    live: true,
    connecting: false,
    notice: null,
    cooldown_ms: 0,
    book: null,
    trades: [],
  })

function socketHarness(clock: FakeClock, symbol = 'ETHUSDC') {
  const net = fakeSockets()
  const frames: string[] = []
  const disconnects: number[] = []
  const socket = createMarketSocket({
    url: 'ws://test',
    symbol,
    deps: {
      open: net.open,
      setTimeout: (fn, ms) => clock.setTimeout(fn, ms),
      clearTimeout: (h) => clock.clearTimeout(h),
      random: () => 0,
    },
    onFrame: (f) => frames.push(f.symbol),
    onDisconnect: (ms) => disconnects.push(ms),
  })
  return { net, frames, disconnects, socket }
}

check('推送：重复 start 不会开第二条连接', async () => {
  const clock = new FakeClock()
  const { net, socket } = socketHarness(clock)
  socket.start()
  socket.start()
  socket.start()
  assertEqual(net.conns.length, 1, '建连次数')
  assertEqual(socket.openConnections, 1, '在用连接')
})

check('推送：断线按指数退避重连，任何时刻最多一条连接', async () => {
  const clock = new FakeClock()
  const { net, disconnects, socket } = socketHarness(clock)
  socket.start()
  for (let i = 0; i < 6; i++) {
    net.drop()
    // 断线后、重连前：没有连接，只有一个重连定时器
    assertEqual(socket.openConnections, 0, `第 ${i + 1} 次断线后`)
    await clock.advance(MAX_BACKOFF_MS, 50)
    assert(net.alive() <= 1, `同时存活 ${net.alive()} 条`)
  }
  assertEqual(net.conns.length, 7, '1 次初连 + 6 次重连')
  assertEqual(disconnects[0], INITIAL_BACKOFF_MS, '首次等待')
  assertEqual(disconnects[1], INITIAL_BACKOFF_MS * 2, '指数增长')
  assert(
    disconnects.every((d) => d <= MAX_BACKOFF_MS),
    `超过上限：${disconnects.join(',')}`,
  )
})

check('推送：连上但没收到帧就断，不算恢复（否则会每秒重连一次）', async () => {
  const clock = new FakeClock()
  const { net, disconnects, socket } = socketHarness(clock)
  socket.start()
  for (let i = 0; i < 3; i++) {
    const c = net.conns.at(-1)
    c?.h.onOpen()
    c?.h.onClose()
    await clock.advance(MAX_BACKOFF_MS, 50)
  }
  assertEqual(disconnects[2], INITIAL_BACKOFF_MS * 4, '只 open 不收帧时退避不复位')

  // 收到帧之后再断：复位
  const c = net.conns.at(-1)
  c?.h.onOpen()
  c?.h.onMessage(FRAME('ETHUSDC'))
  c?.h.onClose()
  assertEqual(disconnects.at(-1), INITIAL_BACKOFF_MS, '收到帧后复位')
})

check('推送：旧连接迟到的 close 不能再触发一次重连', async () => {
  const clock = new FakeClock()
  const { net, socket } = socketHarness(clock)
  socket.start()
  const first = net.conns[0]
  first?.h.onClose()
  await clock.advance(MAX_BACKOFF_MS, 50)
  assertEqual(net.conns.length, 2, '已重连')
  // 第一条连接的 close 事件再来一次（浏览器里 error + close 就是这样）
  first?.h.onClose()
  await clock.advance(MAX_BACKOFF_MS, 50)
  assertEqual(net.conns.length, 2, '迟到事件不得引发第三条连接')
  assertEqual(clock.pending <= 1, true, `定时器 ${clock.pending} 个（只应有心跳）`)
})

check('推送：stop 之后关掉连接、不再重连、不再回调', async () => {
  const clock = new FakeClock()
  const { net, frames, socket } = socketHarness(clock)
  socket.start()
  const c = net.conns[0]
  c?.h.onOpen()
  socket.stop()
  assertEqual(net.alive(), 0, 'stop 应关闭连接')
  c?.h.onMessage(FRAME('ETHUSDC'))
  c?.h.onClose()
  await clock.advance(MAX_BACKOFF_MS * 2, 100)
  assertEqual(net.conns.length, 1, 'stop 后不得重连')
  assertEqual(frames.length, 0, 'stop 后不得回调')
  assertEqual(clock.pending, 0, 'stop 后不留定时器（含心跳）')
})

check('推送：切换交易对期间，旧交易对的迟到帧被丢弃', async () => {
  const clock = new FakeClock()
  const { net, frames, socket } = socketHarness(clock, 'BTCUSDC')
  socket.start()
  net.conns[0]?.h.onMessage(FRAME('ETHUSDC'))
  net.conns[0]?.h.onMessage(FRAME('BTCUSDC'))
  assertEqual(frames.join(','), 'BTCUSDC')
})

// ───────────────────────────────────────────────────────────────────────────
// 8. 请求量实测
// ───────────────────────────────────────────────────────────────────────────

/**
 * 量一下稳态请求量与权重。
 *
 * # 权重从哪来
 *
 * `depth` limit≤50 → 2；`aggTrades` → **20**（最贵的一项）；`klines` 500 根
 * → 5。上限是 **2400 权重/分钟，按 IP 计**——不区分标签页。权重表以币安
 * 文档为准，上限以 `/fapi/v1/exchangeInfo` 的 `rateLimits` 为准。
 *
 * # 事故里的量是怎么来的
 *
 * 一个标签页：盘口 1/s（权重 2）+ 成交流 1/s（权重 20）+ K 线 1/5s（权重 5）
 * = 60×2 + 60×20 + 12×5 = 1380 权重/分钟。重复轮询让它翻倍到 2760，越过
 * 2400 的线 → 429 → 继续请求 → 418。
 *
 * # 现在
 *
 * 盘口与成交流走推送，浏览器不再为它们发任何 REST 请求。剩下的只有 K 线，
 * 用真正的 `createPoller` 按 5 秒间隔跑一分钟来数。（后端对 K 线另有 2 秒
 * 缓存，所以多开标签页也不会线性叠加，这里按单标签页的上界算。）
 */
async function measureRequests(): Promise<void> {
  const clock = new FakeClock()
  const feed = fakeFeed('x')
  const { poller } = harness({ clock, feed, intervalMs: 5_000 })
  poller.start()
  await clock.advance(60_000 - 1, 100)

  const klineCalls = feed.state.calls
  const klineWeight = klineCalls * 5
  assertEqual(klineCalls, 12, 'K 线 5 秒一次，一分钟应为 12 次')
  assertEqual(clock.pending <= 1, true, `稳态下链数应为 1，实际 ${clock.pending}`)

  const limit = 2_400
  const before = 60 * 2 + 60 * 20 + 12 * 5
  const pct = (n: number) => ((n / limit) * 100).toFixed(0)
  console.log(
    `  一个标签页，改造前（1 秒轮询盘口与成交）：\n` +
      `    盘口       60 次/分钟 × 权重 2  = 120\n` +
      `    成交流     60 次/分钟 × 权重 20 = 1200\n` +
      `    K 线       12 次/分钟 × 权重 5  = 60\n` +
      `    合计 ${before} 权重/分钟（${pct(before)}%）；重复轮询翻倍到 ${before * 2}（${pct(before * 2)}%）\n` +
      `  一个标签页，上一版（盘口与成交走推送，K 线仍轮询）：\n` +
      `    盘口 / 成交流   0 次（推送不计 REST 权重）\n` +
      `    K 线     ${klineCalls.toString().padStart(4)} 次/分钟 × 权重 5  = ${klineWeight}\n` +
      `    合计 ${klineWeight} 权重/分钟，占上限 ${limit} 的 ${pct(klineWeight)}%`,
  )
  assert(before * 2 > limit, '反证：改造前的重复轮询量应当越过上限，否则复盘的算术不成立')
  assert(klineWeight * 10 < limit, `十个标签页也应在上限以内，实际单页 ${klineWeight}`)
}

async function main(): Promise<void> {
  let failed = 0
  for (const [name, fn] of checks) {
    try {
      await fn()
      console.log(`  ✓ ${name}`)
    } catch (e) {
      failed += 1
      console.log(`  ✗ ${name}`)
      console.log(`      ${e instanceof Error ? e.message : String(e)}`)
    }
  }

  console.log('\n请求量实测：')
  await measureRequests()

  if (failed > 0) {
    console.log(`\n${failed} 项失败`)
    process.exitCode = 1
  } else {
    console.log(`\n全部 ${checks.length} 项通过`)
  }
}

await main()
