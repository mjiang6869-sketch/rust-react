interface Env {
  META_DB: D1Database;
  MARKET_DATA: R2Bucket;
  BACKTESTS: R2Bucket;
  DOWNLOAD_QUEUE: Queue<DownloadJob>;
  BACKTEST_QUEUE: Queue<BacktestJob>;
  RUST_CRYPTO_INTERNAL_TOKEN?: string;
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

const internalTokenMatches = (request: Request, env: Env): boolean => {
  const provided = request.headers.get("x-rust-crypto-internal-token");
  const configured = env.RUST_CRYPTO_INTERNAL_TOKEN?.trim();
  return configured !== undefined && configured.length > 0 && provided === configured;
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return json({ status: "ok", service: "rust-crypto-api" });
    }
    if (!internalTokenMatches(request, env)) {
      return json({ error: "内部服务令牌无效" }, 401);
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
    if (request.method === "GET" && url.pathname === "/internal/instruments") {
      const productType = url.searchParams.get("product_type");
      const query = productType
        ? env.META_DB.prepare(
            "SELECT * FROM instruments WHERE product_type = ? AND status = 'TRADING' ORDER BY symbol",
          ).bind(productType)
        : env.META_DB.prepare(
            "SELECT * FROM instruments WHERE status = 'TRADING' ORDER BY product_type, symbol",
          );
      return json(await query.all());
    }
    if (request.method === "POST" && url.pathname === "/internal/jobs") {
      const body = (await request.json()) as DownloadJob;
      if (!body.job_id || !body.symbol || !body.interval) {
        return json({ error: "任务字段不完整" }, 400);
      }
      const existing = await env.META_DB.prepare(
        "SELECT job_id, status FROM data_jobs WHERE job_id = ?",
      ).bind(body.job_id).first<{ job_id: string; status: string }>();
      if (existing) return json(existing, 200);
      await env.META_DB.prepare(
        "INSERT INTO data_jobs (job_id, job_type, status, request_json, created_at, updated_at) VALUES (?, 'download', 'queued', ?, datetime('now'), datetime('now'))",
      ).bind(body.job_id, JSON.stringify(body)).run();
      await env.DOWNLOAD_QUEUE.send(body);
      return json({ job_id: body.job_id, status: "queued" }, 202);
    }
    return json({ error: "接口不存在" }, 404);
  },
};
