// 全局限流冷却。
//
// # 为什么必须是全局的
//
// 币安的权重限制**按 IP** 计，不区分是哪个标签页、哪个 hook、哪个组件发的
// 请求。所以「被限流了」是一个进程级事实，不是某个轮询循环的私有状态。
//
// 事故里没有这层：429 被后端包成 500，前端只看到「服务器错误」，于是**继续
// 按原频率重试**。币安明确说过，429 之后继续请求会升级成 418 封禁——实际
// 就升级成了 20 分钟。冷却状态放在模块级单例里，任何一条请求路径撞到 429，
// 所有路径一起停下来。
//
// # 与后端的关系
//
// 后端已经会从币安的 `Retry-After` 响应头解析出真实等待时间，并在自己的
// 429 响应里带回同名的头。所以这里**不需要猜退避时长**——直接读服务端的
// 判断。这一点很重要：指数退避（1s/2s/4s…）在币安这种「秒级精确」的封禁
// 面前是错的工具，它可能远早于解封时刻就重试。

/** 兜底冷却时长：服务端没给 `Retry-After` 时用。 */
export const DEFAULT_COOLDOWN_MS = 5_000

/** 冷却时长上限：1 小时。与服务端 `MAX_RETRY_AFTER_MS` 一致。 */
export const MAX_COOLDOWN_MS = 60 * 60 * 1_000

/** 本进程的冷却截止时刻（毫秒，本地时钟）。0 表示无冷却。 */
let untilMs = 0

/**
 * 从 429 响应里读出服务端要求的等待时间（毫秒）。
 *
 * `Retry-After` 是**秒**。缺失或非数字时退化为 [`DEFAULT_COOLDOWN_MS`]——
 * 一个没有等待时间的 429 不能解释成「立刻重试」。
 */
export function parseRetryAfterMs(header: string | null): number {
  if (header === null) return DEFAULT_COOLDOWN_MS
  const secs = Number(header.trim())
  if (!Number.isFinite(secs) || secs <= 0) return DEFAULT_COOLDOWN_MS
  return Math.min(secs * 1_000, MAX_COOLDOWN_MS)
}

/**
 * 施加冷却，从**现在**开始计时。
 *
 * 取 `max` 而不是覆盖：一条迟到的、等待时间更短的响应不能把一个更长的
 * 封禁截短——那正是「封禁期间继续打」的成因。
 */
export function armCooldown(durationMs: number): void {
  const next = Date.now() + durationMs
  if (next > untilMs) untilMs = next
}

/** 剩余冷却毫秒数。0 表示当前可以发请求。 */
export function cooldownRemainingMs(): number {
  const remaining = untilMs - Date.now()
  if (remaining <= 0) {
    // 到点就清零，让状态回到"无冷却"，避免界面上留着一个过去的截止时刻
    if (untilMs !== 0) untilMs = 0
    return 0
  }
  return remaining
}

/** 是否处于冷却中。 */
export function isCoolingDown(): boolean {
  return cooldownRemainingMs() > 0
}

/**
 * 记录一次成功。
 *
 * **只在冷却已到期时**才清零。否则一条早先发出的请求迟到的成功响应会把
 * 仍在生效的封禁抹掉。
 */
export function clearCooldownIfElapsed(): void {
  if (untilMs !== 0 && untilMs <= Date.now()) untilMs = 0
}

/** 仅测试用：重置冷却状态。 */
export function resetCooldownForTest(): void {
  untilMs = 0
}
