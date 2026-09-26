#!/usr/bin/env bash
#
# 一键启动本地开发环境。
#
# 启动两个进程：
#   1. Rust 服务（API + 引擎），默认 127.0.0.1:8080
#   2. Vite 开发服务器，默认 127.0.0.1:5174（把 /api 代理到后端）
#
# 用法：
#   ./scripts/run.sh            启动（默认）
#   ./scripts/run.sh restart    重启
#   ./scripts/run.sh stop       停止
#   ./scripts/run.sh status     查看状态与健康检查
#   ./scripts/run.sh logs       跟踪日志（Ctrl-C 退出，不会停服务）
#   ./scripts/run.sh build      只构建，不启动
#
# 环境变量（可选）：
#   RUST_CRYPTO_SYMBOL      交易对，默认 ETHUSDC
#   RUST_CRYPTO_MODE        PAPER（默认）或 LIVE
#   RUST_CRYPTO_DATA_ROOT   数据目录，默认 ./data
#   RUST_CRYPTO_BIND        后端监听地址，默认 127.0.0.1:8080
#   RUST_CRYPTO_LOG         日志级别，默认 info
#   RC_RELEASE=1            用 release 构建（首次编译较慢，运行更快）

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${RUST_CRYPTO_RUNTIME_DIR:-$ROOT_DIR/.runtime}"
LOG_DIR="$RUNTIME_DIR/logs"
PID_DIR="$RUNTIME_DIR/pids"

SYMBOL="${RUST_CRYPTO_SYMBOL:-ETHUSDC}"
MODE="${RUST_CRYPTO_MODE:-PAPER}"
DATA_ROOT="${RUST_CRYPTO_DATA_ROOT:-$ROOT_DIR/data}"
BIND="${RUST_CRYPTO_BIND:-127.0.0.1:8080}"
LOG_LEVEL="${RUST_CRYPTO_LOG:-info}"

FRONTEND_PORT="${RUST_CRYPTO_FRONTEND_PORT:-5174}"
FRONTEND_URL="http://127.0.0.1:$FRONTEND_PORT"
API_URL="http://$BIND"

# ---------------------------------------------------------------------------
# 工具链发现
#
# macOS 上从 GUI（Dock、IDE、Spotlight）启动的 shell 不继承登录 shell 的
# PATH，于是 `cargo` 与 `pnpm` 会报 command not found。这里主动补上常见位置。
#
# nvm 需要特别处理：它把每个 Node 版本装在自己的目录下，PATH 依赖
# `nvm use`。而且**不同版本可能装了不同的全局包**——这台机器上只有 v20
# 装了 pnpm，其他版本都没有。所以：
#   1. 优先用 nvm 的 default 别名（尊重用户的选择）
#   2. 其次找装了 pnpm 的版本
#   3. 跳过 Node 14（Vite 6 要求 20+，用 14 会得到难懂的语法错误而非版本提示）
# ---------------------------------------------------------------------------

# 这个版本的 Node 是否满足项目要求（>= 20）。
node_version_ok() {
  local bin="$1"
  [[ -x "$bin/node" ]] || return 1
  local major
  major="$("$bin/node" -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
  [[ "$major" -ge 20 ]]
}

nvm_bin_dir() {
  local base="$HOME/.nvm/versions/node"
  [[ -d "$base" ]] || return 1

  # 1. nvm 的 default 别名
  local default_alias
  default_alias="$(cat "$HOME/.nvm/alias/default" 2>/dev/null || true)"
  if [[ -n "$default_alias" ]]; then
    local dir
    for dir in "$base"/*; do
      [[ -d "$dir/bin" ]] || continue
      case "$(basename "$dir")" in
        "v${default_alias#v}" | "${default_alias#v}" | "v${default_alias#v}".*)
          if node_version_ok "$dir/bin"; then
            printf '%s' "$dir/bin"
            return 0
          fi
          ;;
      esac
    done
  fi

  # 2. 任何装了 pnpm 且版本达标的
  local dir
  for dir in "$base"/*/bin; do
    [[ -x "$dir/pnpm" ]] || continue
    node_version_ok "$dir" || continue
    printf '%s' "$dir"
    return 0
  done

  # 3. 退路：任何版本达标的
  for dir in "$base"/*/bin; do
    node_version_ok "$dir" || continue
    printf '%s' "$dir"
    return 0
  done

  return 1
}

setup_path() {
  # Rust
  if [[ -d "$HOME/.cargo/bin" ]]; then
    PATH="$HOME/.cargo/bin:$PATH"
  fi

  # Node（nvm）
  local nb
  if nb="$(nvm_bin_dir)"; then
    PATH="$nb:$PATH"
  fi

  # 常见包管理器位置
  local dir
  for dir in "$HOME/Library/pnpm" /opt/homebrew/bin /usr/local/bin "$HOME/.local/share/pnpm"; do
    if [[ -d "$dir" && ":$PATH:" != *":$dir:"* ]]; then
      PATH="$dir:$PATH"
    fi
  done

  export PATH
}

# ---------------------------------------------------------------------------
# 输出
# ---------------------------------------------------------------------------
if [[ -t 1 ]]; then
  C_RESET=$'\033[0m'; C_DIM=$'\033[2m'; C_BOLD=$'\033[1m'
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[34m'
else
  C_RESET=''; C_DIM=''; C_BOLD=''; C_RED=''; C_GREEN=''; C_YELLOW=''; C_BLUE=''
fi

info()  { printf '%s\n' "$*"; }
ok()    { printf '%s✓%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()  { printf '%s!%s %s\n' "$C_YELLOW" "$C_RESET" "$*"; }
bad()   { printf '%s✗%s %s\n' "$C_RED" "$C_RESET" "$*"; }
step()  { printf '\n%s%s%s\n' "$C_BOLD" "$*" "$C_RESET"; }
dim()   { printf '%s%s%s\n' "$C_DIM" "$*" "$C_RESET"; }

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    bad "缺少命令：$1"
    case "$1" in
      cargo) info "  安装 Rust：https://rustup.rs" ;;
      node|pnpm)
        info "  安装 Node.js 20+：https://nodejs.org"
        info "  安装 pnpm：corepack enable pnpm"
        ;;
    esac
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# 进程管理
#
# 只管理本脚本启动的进程，用 pid 文件记录。不靠进程名匹配——那会误杀其他
# 项目里同名的进程。
#
# # 为什么记录进程组而不是单个 PID
#
# Vite 会派生 esbuild 子进程。只杀父进程会留下孤儿继续占用端口，下次启动
# 就会报「端口被占用」，而用户看不到任何残留进程（`pgrep vite` 也找不到
# esbuild）。所以停止时对**整个进程组**发信号。
#
# bash 的作业控制（`set -m`）让每个后台任务进入自己的进程组，于是记录的
# PID 就是组长 PID，可以用 `kill -- -PID` 对整组发信号。
# ---------------------------------------------------------------------------
pid_file() { printf '%s/%s.pid' "$PID_DIR" "$1"; }

is_alive() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

read_pid() {
  local f
  f="$(pid_file "$1")"
  if [[ -f "$f" ]]; then
    tr -d ' \n' < "$f" 2>/dev/null || true
  fi
}

write_pid() {
  mkdir -p "$PID_DIR"
  # 写换行：多个 pid 文件用 `cat pids/*.pid` 查看时不会连成一串
  printf '%s\n' "$2" > "$(pid_file "$1")"
}

clear_pid() { rm -f "$(pid_file "$1")"; }

# 结束一个进程组（含全部子孙）。
kill_group() {
  local pgid="$1"
  if ! is_alive "$pgid"; then
    return 0
  fi

  # 负号表示对整个进程组发信号
  kill -TERM -- "-$pgid" 2>/dev/null || kill -TERM "$pgid" 2>/dev/null || true

  local waited=0
  while is_alive "$pgid" && (( waited < 40 )); do
    sleep 0.25
    waited=$((waited + 1))
  done

  if is_alive "$pgid"; then
    warn "进程组 $pgid 未响应 TERM，强制结束"
    kill -KILL -- "-$pgid" 2>/dev/null || kill -KILL "$pgid" 2>/dev/null || true
  fi
}

# 启动一个后台服务，返回其进程组 ID。
spawn_service() {
  local name="$1" log="$2"
  shift 2

  mkdir -p "$PID_DIR" "$LOG_DIR"
  : > "$log"

  # 作业控制：让后台任务进入自己的进程组
  set -m
  (
    cd "$ROOT_DIR"
    exec "$@"
  ) >>"$log" 2>&1 &
  local pid=$!
  set +m

  write_pid "$name" "$pid"
  printf '%s' "$pid"
}

# ---------------------------------------------------------------------------
# 端口检查
#
# 端口被占用时给出明确提示，而不是让服务抛一个难懂的绑定错误。
# ---------------------------------------------------------------------------
port_owner() {
  lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -1 || true
}

check_port_free() {
  local port="$1" name="$2"
  local owner
  owner="$(port_owner "$port")"
  if [[ -n "$owner" ]]; then
    bad "$name 端口 $port 已被占用（PID ${owner}）"
    info "  查看占用者：lsof -nP -iTCP:$port -sTCP:LISTEN"
    info "  或换个端口：RUST_CRYPTO_BIND / RUST_CRYPTO_FRONTEND_PORT"
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------------------
# 健康检查
# ---------------------------------------------------------------------------
wait_for_api() {
  local tries="${1:-120}" i=0
  while (( i < tries )); do
    if curl -fsS --max-time 1 "$API_URL/api/v1/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
    i=$((i + 1))
  done
  return 1
}

wait_for_frontend() {
  local tries="${1:-120}" i=0
  while (( i < tries )); do
    if curl -fsS --max-time 1 "$FRONTEND_URL/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
    i=$((i + 1))
  done
  return 1
}

# ---------------------------------------------------------------------------
# 构建与启动
# ---------------------------------------------------------------------------
binary_path() {
  if [[ "${RC_RELEASE:-0}" == "1" ]]; then
    printf '%s/target/release/rust-crypto' "$ROOT_DIR"
  else
    printf '%s/target/debug/rust-crypto' "$ROOT_DIR"
  fi
}

do_cargo_build() {
  if [[ "${RC_RELEASE:-0}" == "1" ]]; then
    ( cd "$ROOT_DIR" && cargo build --release --bin rust-crypto )
  else
    ( cd "$ROOT_DIR" && cargo build --bin rust-crypto )
  fi
}

start_rust() {
  local pid
  pid="$(read_pid rust)"
  if is_alive "$pid"; then
    ok "Rust 服务已在运行（PID ${pid}）"
    return 0
  fi
  clear_pid rust

  # BIND 形如 127.0.0.1:8080，取最后一段作端口
  local port="${BIND##*:}"
  check_port_free "$port" "后端" || return 1

  mkdir -p "$DATA_ROOT"
  local log="$LOG_DIR/rust.log"

  info "启动 Rust 服务（$SYMBOL / $MODE / ${BIND}）"
  pid="$(spawn_service rust "$log" env \
    RUST_CRYPTO_SYMBOL="$SYMBOL" \
    RUST_CRYPTO_MODE="$MODE" \
    RUST_CRYPTO_DATA_ROOT="$DATA_ROOT" \
    RUST_CRYPTO_BIND="$BIND" \
    RUST_LOG="$LOG_LEVEL" \
    "$(binary_path)")"

  if ! wait_for_api; then
    bad "Rust 服务启动失败或超时"
    info "  日志尾部："
    tail -25 "$log" 2>/dev/null | sed 's/^/    /' || true
    kill_group "$pid"
    clear_pid rust
    return 1
  fi
  ok "Rust 服务就绪（PID ${pid}，${API_URL}）"
}

start_frontend() {
  local pid
  pid="$(read_pid frontend)"
  if is_alive "$pid"; then
    ok "前端已在运行（PID ${pid}）"
    return 0
  fi
  clear_pid frontend

  check_port_free "$FRONTEND_PORT" "前端" || return 1

  local log="$LOG_DIR/frontend.log"

  # 把后端地址传给 Vite，而不是让它用硬编码的 8080。
  #
  # 端口被占用时用户会换端口（RUST_CRYPTO_BIND），硬编码会让代理指向错的
  # 地方，而界面上只显示「无法连接后端」——原因看不出来。
  info "启动前端开发服务器（${FRONTEND_URL} → 代理到 ${API_URL}）"
  pid="$(spawn_service frontend "$log" env \
    RUST_CRYPTO_API_URL="$API_URL" \
    pnpm --dir "$ROOT_DIR/frontend" dev --port "$FRONTEND_PORT")"

  if ! wait_for_frontend; then
    bad "前端启动失败或超时"
    info "  日志尾部："
    tail -25 "$log" 2>/dev/null | sed 's/^/    /' || true
    kill_group "$pid"
    clear_pid frontend
    return 1
  fi
  ok "前端就绪（PID ${pid}，${FRONTEND_URL}）"
}

cmd_build() {
  step "环境检查"
  require_command cargo || exit 1
  require_command node || exit 1
  require_command pnpm || exit 1

  step "构建 Rust 服务"
  do_cargo_build
  ok "Rust 服务构建完成"

  step "检查前端"
  if [[ ! -d "$ROOT_DIR/frontend/node_modules" ]]; then
    info "  首次运行，安装依赖…"
    ( cd "$ROOT_DIR/frontend" && pnpm install )
  fi
  ( cd "$ROOT_DIR/frontend" && pnpm typecheck )
  ok "前端类型检查通过"
}

cmd_start() {
  step "环境检查"
  local missing=0
  require_command cargo || missing=1
  require_command node  || missing=1
  require_command pnpm  || missing=1
  require_command curl  || missing=1
  (( missing == 0 )) || exit 1
  ok "cargo $(cargo --version 2>/dev/null | awk '{print $2}')"
  ok "node  $(node --version)"

  # 依赖缺失时先装，避免启动后才报错
  if [[ ! -d "$ROOT_DIR/frontend/node_modules" ]]; then
    step "安装前端依赖"
    ( cd "$ROOT_DIR/frontend" && pnpm install )
  fi

  # 每次启动都交给 cargo 判断是否需要重编：代码没变时一秒内结束。只在二进制
  # 不存在时才编译会让 restart 一直跑旧二进制——改了代码重启后新接口 404。
  if [[ ! -x "$(binary_path)" ]]; then
    step "构建 Rust 服务（首次较慢）"
  else
    step "构建 Rust 服务（代码没变时很快）"
  fi
  do_cargo_build || exit 1
  ok "构建完成"

  step "启动服务"
  start_rust || exit 1
  start_frontend || exit 1

  step "就绪"
  info "  界面      ${C_BLUE}$FRONTEND_URL${C_RESET}"
  info "  API       $API_URL/api/v1/health"
  info "  数据      $DATA_ROOT"
  info "  日志      $LOG_DIR/"
  info ""
  if [[ "$MODE" == "LIVE" ]]; then
    warn "当前是 LIVE 模式。真实下单还需要在界面里显式开启（ARM）。"
  else
    dim "  当前是模拟盘（PAPER）。实盘需显式设置 RUST_CRYPTO_MODE=LIVE。"
  fi
  dim "  停止：./scripts/run.sh stop    查看状态：./scripts/run.sh status"
}

cmd_stop() {
  step "停止服务"
  local stopped=0
  local name pid
  for name in frontend rust; do
    pid="$(read_pid "$name")"
    if is_alive "$pid"; then
      info "  停止 ${name}（PID ${pid}）"
      kill_group "$pid"
      stopped=$((stopped + 1))
    fi
    clear_pid "$name"
  done

  if (( stopped == 0 )); then
    dim "  没有本脚本启动的进程在运行"
  else
    ok "已停止 $stopped 个进程"
  fi
}

cmd_status() {
  step "进程状态"
  local name pid
  for name in rust frontend; do
    pid="$(read_pid "$name")"
    if is_alive "$pid"; then
      ok "$name 运行中（PID ${pid}）"
    else
      bad "$name 未运行"
    fi
  done

  step "健康检查"
  if curl -fsS --max-time 2 "$API_URL/api/v1/health" >/dev/null 2>&1; then
    ok "API 正常（${API_URL}）"
    if curl -fsS --max-time 2 "$API_URL/api/v1/state" 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)["data"]
print(f"  模式      {d[\"mode\"]}（{d[\"mode_label\"]}）")
print(f"  交易对    {d[\"symbol\"]}")
print(f"  权益      {d[\"equity\"]}")
feed = "正常" if d["feed_fresh"] else ("已连接但陈旧" if d["feed_connected"] else "未连接")
print(f"  行情      {feed}")
print(f"  持仓      {"有" if d["position"] else "无"}")
print(f"  在途单    {len(d[\"open_orders\"])} 张")
print(f"  成交模型  {d[\"fill_model\"]}")
' 2>/dev/null; then
      :
    fi
  else
    bad "API 无响应（$API_URL/api/v1/health）"
  fi

  if curl -fsS --max-time 2 "$FRONTEND_URL/" >/dev/null 2>&1; then
    ok "前端可访问（${FRONTEND_URL}）"
  else
    bad "前端无响应（${FRONTEND_URL}）"
  fi

  step "日志文件"
  local f
  for f in rust frontend; do
    if [[ -f "$LOG_DIR/$f.log" ]]; then
      info "  $f: $(wc -l < "$LOG_DIR/$f.log" | tr -d ' ') 行  $LOG_DIR/$f.log"
    fi
  done
}

cmd_logs() {
  mkdir -p "$LOG_DIR"
  touch "$LOG_DIR/rust.log" "$LOG_DIR/frontend.log"
  step "跟踪日志（Ctrl-C 退出，不会停止服务）"
  dim "  $LOG_DIR/rust.log 与 $LOG_DIR/frontend.log"
  tail -n 40 -f "$LOG_DIR/rust.log" "$LOG_DIR/frontend.log" || true
}

cmd_restart() {
  cmd_stop
  cmd_start
}

usage() {
  cat <<'EOF'
用法：scripts/run.sh [命令]

命令：
  start      启动 Rust 服务与前端（默认）
  stop       停止本脚本启动的进程
  restart    重启
  status     查看进程状态与健康检查
  logs       跟踪日志
  build      只构建，不启动
  help       显示本说明

环境变量：
  RUST_CRYPTO_SYMBOL         交易对，默认 ETHUSDC
  RUST_CRYPTO_MODE           PAPER（默认）或 LIVE
  RUST_CRYPTO_DATA_ROOT      数据目录，默认 ./data
  RUST_CRYPTO_BIND           后端监听地址，默认 127.0.0.1:8080
  RUST_CRYPTO_FRONTEND_PORT  前端端口，默认 5174
  RUST_CRYPTO_LOG            日志级别，默认 info
  RC_RELEASE=1               用 release 构建

示例：
  ./scripts/run.sh
  RUST_CRYPTO_SYMBOL=BTCUSDC ./scripts/run.sh
  RC_RELEASE=1 ./scripts/run.sh restart
EOF
}

main() {
  setup_path
  mkdir -p "$RUNTIME_DIR"

  case "${1:-start}" in
    start)   cmd_start ;;
    stop)    cmd_stop ;;
    restart) cmd_restart ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    build)   cmd_build ;;
    help | -h | --help) usage ;;
    *)
      bad "未知命令：$1"
      info ""
      usage
      exit 2
      ;;
  esac
}

main "$@"
