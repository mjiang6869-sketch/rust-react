CREATE TABLE IF NOT EXISTS market_data_gaps (
  dataset_id TEXT NOT NULL,
  gap_start_utc TEXT NOT NULL,
  gap_end_utc TEXT NOT NULL,
  gap_type TEXT NOT NULL,
  PRIMARY KEY (dataset_id, gap_start_utc)
);

CREATE TABLE IF NOT EXISTS backtest_runs (
  run_id TEXT PRIMARY KEY,
  dataset_id TEXT NOT NULL,
  strategy_version TEXT NOT NULL,
  config_hash TEXT NOT NULL,
  result_object_prefix TEXT,
  status TEXT NOT NULL,
  started_at TEXT,
  completed_at TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_backtest_runs_status
  ON backtest_runs (status, created_at);
