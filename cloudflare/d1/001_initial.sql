CREATE TABLE IF NOT EXISTS market_datasets (
  dataset_id TEXT PRIMARY KEY,
  venue TEXT NOT NULL,
  product_type TEXT NOT NULL,
  symbol TEXT NOT NULL,
  interval TEXT NOT NULL,
  start_time_utc TEXT NOT NULL,
  end_time_utc TEXT NOT NULL,
  row_count INTEGER NOT NULL,
  object_prefix TEXT NOT NULL,
  checksum TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  completeness TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS data_jobs (
  job_id TEXT PRIMARY KEY,
  job_type TEXT NOT NULL,
  status TEXT NOT NULL,
  request_json TEXT NOT NULL,
  result_object_prefix TEXT,
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_market_datasets_lookup
  ON market_datasets (venue, product_type, symbol, interval, start_time_utc);
CREATE INDEX IF NOT EXISTS idx_data_jobs_status
  ON data_jobs (status, created_at);
