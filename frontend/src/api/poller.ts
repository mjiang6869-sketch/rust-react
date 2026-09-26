// 轮询状态机。
//
// # 为什么单独一个文件、不放在 hook 里
//
// 事故里的重复轮询（切一次标签页，定时器链 +1）不是 React 的问题，是
// 「谁负责排下一轮」没有单一答案的问题。把它从 hook 里抽出来有两个直接
// 好处（现在只有 K 线用它；盘口与成交流已改走推送，见 `marketSocket.ts`）：
//
// 1. **可离线复现**。这段逻辑不碰 React、不碰 DOM，只依赖注入进来的
//    `setTimeout`/`clearTimeout`/`now`。测试脚本可以用假时钟驱动它，
//    直接数"同时存在的定时器有几条"——正是用户当初复现事故的办法。
// 2. **排程点只有一个**。全文只有 [`PollLoop.schedule`] 会写 `timer`，
//    因此「两个定时器 = 两条链」这件事在代码层面就不可能出现。
//
// # 状态的含义
//
// `timer`      —— 已排定的下一轮（`null` 表示没排）
// `inflight`   —— 有没有一轮正在 await 请求
// `pending`    —— 「立刻补一次」的意图，由在途的那一轮消费
// `nextDelayMs`—— 下一轮等多久（限流时是剩余冷却时间）
//
// # 与限流的关系
//
// 冷却期内**不发请求**，并把下一次延迟拉到冷却结束之后。币安明确规定：
// 429 之后继续请求会升级成 418 封禁。事故里前端没有这层，退化成"按原
// 频率硬重试"，于是 2 分钟的限流变成了 20 分钟的封禁。

/** 冷却状态查询。注入进来以便离线测试伪造限流。 */
export interface Clock {
  /** 当前时刻（毫秒）。 */
  now(): number
  /** 剩余冷却毫秒数，0 表示可以发请求。 */
  cooldownRemainingMs(): number
  setTimeout(fn: () => void, ms: number): ReturnType<typeof setTimeout>
  clearTimeout(handle: ReturnType<typeof setTimeout>): void
}

export interface PollLoop {
  /** 已排定的下一轮。 */
  timer: ReturnType<typeof setTimeout> | null
  /** 已停止：不再排程。 */
  stopped: boolean
  /** 有没有一轮正在 await 请求。 */
  inflight: boolean
  /** 「立刻补一次」的意图。 */
  pending: boolean
  /** 下一轮的等待时长（毫秒）。 */
  nextDelayMs: number
  /** 当前同时存在的定时器条数。正常情况下只可能是 0 或 1。 */
  readonly timerCount: number
}

export interface PollOptions<T> {
  /** 正常轮询间隔。 */
  intervalMs: number
  /** 一次抓取。返回值交给 `onData`。 */
  fetch: () => Promise<T>
  /** 成功：更新界面。 */
  onData: (data: T) => void
  /** 失败：显示原因。`limited` 为真表示这次失败是限流。 */
  onError: (message: string, limited: boolean) => void
  /**
   * 冷却中：不发请求，只更新提示。
   *
   * 只给剩余毫秒，**不给文案**——措辞由界面决定（盘口与 K 线用的是同一个
   * `cooldownNotice`）。让这个模块负责文案会把 UI 关注点混进来。
   */
  onCooldown: (remainingMs: number) => void
  /** 把异常转成给用户看的中文。 */
  describe: (e: unknown) => string
  /** 判断一个异常是不是限流。 */
  isLimited: (e: unknown) => boolean
  /** 是否暂停（页面不可见）。 */
  isPaused: () => boolean
  clock: Clock
}

export interface Poller {
  /** 启动（首次立刻跑一轮）。 */
  start(): void
  /** 停止：取消定时器，忽略在途结果，不再排程。 */
  stop(): void
  /** 页面可见性变化时调用。 */
  onVisibilityChange(): void
  /** 供测试观察内部状态。 */
  readonly loop: PollLoop
  /** 立即执行一轮（测试用）。 */
  runOnce(): Promise<void>
}

/**
 * 建立轮询循环。返回的对象不持有 DOM 或 React 引用。
 */
export function createPoller<T>(opts: PollOptions<T>): Poller {
  const { clock, intervalMs } = opts

  const loop: PollLoop = {
    timer: null,
    stopped: false,
    inflight: false,
    pending: false,
    nextDelayMs: intervalMs,
    get timerCount() {
      return loop.timer === null ? 0 : 1
    },
  }

  /**
   * 排下一轮。**全文件唯一的排程点。**
   *
   * 已经排好就不重复排——两个定时器就是两条链，这是事故的根因，
   * 所以在唯一的入口上直接堵死。
   */
  const schedule = (): void => {
    if (loop.stopped) return
    if (loop.timer !== null) return
    loop.timer = clock.setTimeout(() => {
      // 定时器已触发，句柄作废。**必须在这里置 `null`**，否则 tick 收尾
      // 时 `schedule` 会因为 `timer !== null` 而拒绝排下一轮，轮询停死。
      loop.timer = null
      void tick()
    }, loop.nextDelayMs)
  }

  const cancelScheduled = (): void => {
    if (loop.timer !== null) {
      clock.clearTimeout(loop.timer)
      loop.timer = null
    }
  }

  const tick = async (): Promise<void> => {
    if (loop.stopped) return

    // 页面不可见：不排下一轮，直接停。切回来由 `onVisibilityChange` 重启。
    // 旧写法是"不可见也照排、只是跳过请求"，那会留下一条每秒空转的链。
    if (opts.isPaused()) return

    // 冷却期内不发请求。
    const cooling = clock.cooldownRemainingMs()
    if (cooling > 0) {
      opts.onCooldown(cooling)
      loop.nextDelayMs = cooling + intervalMs
      schedule()
      return
    }
    loop.nextDelayMs = intervalMs

    if (loop.inflight) {
      // 正常情况下 `schedule` 的单一排程点已经排除这种可能。真发生时
      // 也绝不并发再发一轮，只把意图留给在途的那一轮。
      loop.pending = true
      return
    }
    loop.inflight = true

    let limited = false
    try {
      const data = await opts.fetch()
      if (loop.stopped) return
      opts.onData(data)
    } catch (e) {
      if (loop.stopped) return
      limited = opts.isLimited(e)
      opts.onError(opts.describe(e), limited)
    } finally {
      loop.inflight = false
      if (!loop.stopped) {
        // 下一轮延迟在**本轮结束之后**决定：撞到限流就等冷却结束。
        // 这是与 `setInterval` 最本质的区别——延迟与请求耗时挂钩了。
        const now = clock.cooldownRemainingMs()
        if (limited || now > 0) {
          loop.pending = false
          loop.nextDelayMs = (now > 0 ? now : intervalMs) + intervalMs
        } else if (loop.pending) {
          loop.pending = false
          loop.nextDelayMs = 0
        } else {
          loop.nextDelayMs = intervalMs
        }
        schedule()
      }
    }
  }

  return {
    start(): void {
      void tick()
    },
    stop(): void {
      loop.stopped = true
      cancelScheduled()
    },
    onVisibilityChange(): void {
      if (opts.isPaused()) {
        // 切走：停掉链，后台不空转
        cancelScheduled()
        return
      }
      cancelScheduled()
      if (loop.inflight) {
        // 有在途请求：只记意图，由它收尾时立刻补一次。
        // **不在这里直接 tick** —— 那正是重复轮询的成因。
        loop.pending = true
      } else {
        void tick()
      }
    },
    loop,
    runOnce(): Promise<void> {
      return tick()
    },
  }
}
