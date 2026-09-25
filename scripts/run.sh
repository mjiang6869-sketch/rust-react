#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${RUST_CRYPTO_RUNTIME_DIR:-$ROOT_DIR/.runtime}"
PID_FILE="$RUNTIME_DIR/electron.pid"
LOG_FILE="$RUNTIME_DIR/desktop.log"

usage() {
  cat <<'EOF'
用法：scripts/run.sh [start|restart|stop|status|logs|cloudflare-check|cloudflare-migrate|cloudflare-deploy]

默认命令是 start。start 会编译 Rust sidecar，并由 Electron 启动 Rust API 和 Vite renderer。
restart 会先停止本脚本启动的 Electron 进程及其子进程，再重新启动。
EOF
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    printf '缺少命令：%s\n' "$1" >&2
    exit 1
  }
}

read_pid() {
  [[ -f "$PID_FILE" ]] || return 1
  local pid
  pid="$(<"$PID_FILE")"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$pid"
}

is_alive() {
  local pid="$1"
  kill -0 "$pid" 2>/dev/null
}

kill_tree() {
  local pid="$1"
  local child
  while read -r child; do
    [[ -n "$child" ]] && kill_tree "$child"
  done < <(pgrep -P "$pid" 2>/dev/null || true)
  kill "$pid" 2>/dev/null || true
}

stop_app() {
  local pid
  if ! pid="$(read_pid)" || ! is_alive "$pid"; then
    rm -f "$PID_FILE"
    printf '当前没有由脚本记录的运行实例。\n'
    return 0
  fi
  kill_tree "$pid"
  for _ in {1..40}; do
    is_alive "$pid" || break
    sleep 0.25
  done
  if is_alive "$pid"; then
    kill -KILL "$pid" 2>/dev/null || true
  fi
  rm -f "$PID_FILE"
  printf '已停止 Electron、Rust sidecar 和 renderer。\n'
}

build_app() {
  require_command cargo
  require_command node
  require_command pnpm
  require_command curl
  if [[ ! -x "$ROOT_DIR/frontend/node_modules/.bin/vite" ]]; then
    (cd "$ROOT_DIR/frontend" && pnpm install --frozen-lockfile)
  fi
  if [[ ! -d "$ROOT_DIR/desktop/node_modules/electron" ]]; then
    (cd "$ROOT_DIR/desktop" && pnpm install)
  fi
  (cd "$ROOT_DIR" && cargo build)
}

start_app() {
  mkdir -p "$RUNTIME_DIR"
  if local_pid="$(read_pid 2>/dev/null || true)"; [[ -n "$local_pid" ]] && is_alive "$local_pid"; then
    printf '应用已经运行，PID=%s，健康检查：http://127.0.0.1:8080/api/health\n' "$local_pid"
    return 0
  fi
  build_app
  rm -f "$LOG_FILE"
  (cd "$ROOT_DIR" && pnpm --dir desktop dev) >"$LOG_FILE" 2>&1 &
  local pid=$!
  printf '%s\n' "$pid" >"$PID_FILE"
  for _ in {1..60}; do
    if curl -fsS http://127.0.0.1:8080/api/health >/dev/null 2>&1; then
      printf '已启动 Electron，PID=%s。健康检查通过。\n' "$pid"
      printf '日志：%s\n' "$LOG_FILE"
      return 0
    fi
    if ! is_alive "$pid"; then
      printf 'Electron 启动失败，最近日志：\n' >&2
      tail -n 80 "$LOG_FILE" >&2 || true
      rm -f "$PID_FILE"
      exit 1
    fi
    sleep 0.25
  done
  printf '等待 Rust API 健康检查超时，日志：%s\n' "$LOG_FILE" >&2
  exit 1
}

cloudflare_check() {
  require_command node
  require_command pnpm
  (cd "$ROOT_DIR/cloudflare/worker" && pnpm install --frozen-lockfile && pnpm typecheck)
}

cloudflare_deploy() {
  cloudflare_check
  (cd "$ROOT_DIR/cloudflare/worker" && pnpm deploy)
}

cloudflare_migrate() {
  require_command pnpm
  (cd "$ROOT_DIR/cloudflare/worker" && pnpm exec wrangler d1 execute rust-crypto-meta --remote --file=../d1/001_initial.sql)
  (cd "$ROOT_DIR/cloudflare/worker" && pnpm exec wrangler d1 execute rust-crypto-meta --remote --file=../d1/002_operational_tables.sql)
}

command_name="${1:-start}"
case "$command_name" in
  start) start_app ;;
  restart) stop_app; start_app ;;
  stop) stop_app ;;
  status)
    if pid="$(read_pid 2>/dev/null || true)" && [[ -n "$pid" ]] && is_alive "$pid"; then
      printf '运行中，PID=%s\n' "$pid"
      curl -fsS http://127.0.0.1:8080/api/health || true
      printf '\n'
    else
      printf '未运行。\n'
      exit 1
    fi
    ;;
  logs) touch "$LOG_FILE"; tail -f "$LOG_FILE" ;;
  cloudflare-check) cloudflare_check ;;
  cloudflare-migrate) cloudflare_migrate ;;
  cloudflare-deploy) cloudflare_deploy ;;
  help|-h|--help) usage ;;
  *) usage >&2; exit 2 ;;
esac
