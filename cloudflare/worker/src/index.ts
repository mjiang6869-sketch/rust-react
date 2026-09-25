interface Env {
  META_DB: D1Database;
  MARKET_DATA: R2Bucket;
  BACKTESTS: R2Bucket;
  DOWNLOAD_QUEUE: Queue<DownloadJob>;
  BACKTEST_QUEUE: Queue<BacktestJob>;
}

interface DownloadJob {
  job_id: string;
  symbol: string;
  interval: string;
}

interface BacktestJob {
  job_id: string;
  run_id: string;
  dataset_id: string;
}

const json = (body: unknown, status = 200): Response =>
  Response.json(body, { status, headers: { "cache-control": "no-store" } });

const internalTokenMatches = (request: Request): boolean => {
  const configured = request.headers.get("x-rust-crypto-internal-token");
  return configured !== null && configured.length > 0;
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return json({ status: "ok", service: "rust-crypto-api" });
    }
    if (!internalTokenMatches(request)) {
      return json({ error: "缺少内部服务令牌" }, 401);
    }
    if (request.method === "GET" && url.pathname === "/internal/datasets") {
      const symbol = url.searchParams.get("symbol");
      const query = symbol
        ? env.META_DB.prepare(
            "SELECT * FROM market_datasets WHERE symbol = ? ORDER BY start_time_utc DESC LIMIT 100",
          ).bind(symbol)
        : env.META_DB.prepare(
            "SELECT * FROM market_datasets ORDER BY start_time_utc DESC LIMIT 100",
          );
      return json(await query.all());
    }
    if (request.method === "POST" && url.pathname === "/internal/jobs") {
      const body = (await request.json()) as DownloadJob;
      if (!body.job_id || !body.symbol || !body.interval) {
        return json({ error: "任务字段不完整" }, 400);
      }
      await env.META_DB.prepare(
        "INSERT INTO data_jobs (job_id, job_type, status, request_json, created_at, updated_at) VALUES (?, 'download', 'queued', ?, datetime('now'), datetime('now'))",
      ).bind(body.job_id, JSON.stringify(body)).run();
      await env.DOWNLOAD_QUEUE.send(body);
      return json({ job_id: body.job_id, status: "queued" }, 202);
    }
    return json({ error: "接口不存在" }, 404);
  },
};
