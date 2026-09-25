//! 下载台账。
//!
//! # 为什么需要它
//!
//! 历史数据下载是**长任务**（3 标的 32 个月约 12 GB），必须能中断、恢复、
//! 且知道"哪些已经有、哪些有缺口"。台账就是这个记忆。
//!
//! # 设计要点
//!
//! 1. **`Finalized` 是核心状态**：分区下载并校验通过后标记，永不再下。
//!    行数与粒度期望不符时**不标记通过**，而是记为待查——宁可重复下载，
//!    也不能把不完整的数据当成完整数据用。
//! 2. **保留原始包的 sha256 与来源 URL**：币安偶尔回填修正历史。有了这两项，
//!    随时能验证本地 Parquet 是否还对应归档内容，也能重新获取原始包。
//! 3. **缺口是显式记录而非静默跳过**：回测跨越缺口会凭空发明不可能的成交，
//!    所以缺口必须能让回测拒绝运行。
//! 4. **原子替换 + 拒绝未知版本**：沿用旧 `storage.rs` 的正确做法，但补上
//!    真正的迁移梯子——旧实现靠 serde `default` 静默填充缺失字段，等价于
//!    "格式变了也当没变"，正是要避免的静默错解。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// 台账格式版本。变更结构时递增，并在 `migrate` 里补上迁移分支。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 数据集种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetKind {
    /// 1 分钟 K 线。
    Klines1m,
    /// 逐笔聚合成交。M1 成交模型与 markout 的唯一数据源。
    AggTrades,
    /// 1 分钟标记价 K 线。用于估算强平距离。
    MarkPriceKlines1m,
    /// 资金费率。8 小时一次。
    FundingRate,
}

impl DatasetKind {
    /// 币安归档里的路径片段（`data/futures/um/{monthly|daily}/` 之后）。
    pub fn archive_dir(self) -> &'static str {
        match self {
            DatasetKind::Klines1m => "klines",
            DatasetKind::AggTrades => "aggTrades",
            DatasetKind::MarkPriceKlines1m => "markPriceKlines",
            DatasetKind::FundingRate => "fundingRate",
        }
    }

    /// 归档文件名里的类型标记。`ETHUSDC-{tag}-2026-08.zip`。
    pub fn file_tag(self) -> &'static str {
        match self {
            DatasetKind::Klines1m => "1m",
            DatasetKind::AggTrades => "aggTrades",
            DatasetKind::MarkPriceKlines1m => "1m",
            DatasetKind::FundingRate => "fundingRate",
        }
    }

    /// K 线类数据的每日期望行数。逐笔数据没有固定期望，返回 `None`。
    pub fn expected_rows_per_day(self) -> Option<u64> {
        match self {
            // 1 分钟 K 线：一天 1440 根。
            DatasetKind::Klines1m | DatasetKind::MarkPriceKlines1m => Some(1440),
            // 资金费每 8 小时一次：一天 3 次。
            DatasetKind::FundingRate => Some(3),
            // 逐笔成交的笔数取决于市场活跃度，没有期望值。
            DatasetKind::AggTrades => None,
        }
    }

    /// 该数据集的归档是否只在 monthly 路径提供。
    ///
    /// `bookTicker` 曾在 monthly/daily 都提供，但 2024-04 后停更，已从
    /// 支持列表移除（见 crates/data/README.md）。
    pub fn monthly_only(self) -> bool {
        false
    }
}

/// 一个下载分区：某数据集的某标的某个月。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PartitionKey {
    pub kind: DatasetKind,
    pub symbol: String,
    /// `(年, 月)`。
    pub year: i32,
    pub month: u32,
}

impl std::fmt::Display for PartitionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}/{}/{}-{:02}",
            self.kind, self.symbol, self.year, self.month
        )
    }
}

/// 分区的下载与校验状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PartitionStatus {
    /// 还未处理。
    Absent,
    /// 归档里不存在这个分区（例如标的尚未上线）。这是终态，不再重试。
    NotInArchive,
    /// 已下载并转换，校验通过。
    Finalized {
        row_count: u64,
        min_ts: DateTime<Utc>,
        max_ts: DateTime<Utc>,
    },
    /// 已下载转换，但行数与期望不符。**不算通过**，需要人工查看。
    Suspicious {
        row_count: u64,
        expected: u64,
        note: String,
    },
    /// 下载或转换失败。
    Failed { error: String, attempts: u32 },
}

impl PartitionStatus {
    /// 是否可以跳过下载。
    pub fn is_done(&self) -> bool {
        matches!(
            self,
            PartitionStatus::Finalized { .. } | PartitionStatus::NotInArchive
        )
    }

    /// 数据是否可用于回测。
    pub fn is_usable(&self) -> bool {
        matches!(self, PartitionStatus::Finalized { .. })
    }
}

/// 一个分区的台账记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionEntry {
    pub status: PartitionStatus,
    /// 原始归档 ZIP 的 SHA-256。用于验证本地派生物仍对应归档内容。
    pub source_sha256: Option<String>,
    pub source_url: Option<String>,
    /// 转换后 Parquet 的相对路径（相对 data root）。
    pub parquet_path: Option<PathBuf>,
    pub parquet_bytes: Option<u64>,
    /// 首次成功获取的时刻。
    pub fetched_at: Option<DateTime<Utc>>,
    /// 归档自那次获取后是否被币安修订（重新下载时对比 sha256 得知）。
    pub restated_at: Option<DateTime<Utc>>,
}

impl PartitionEntry {
    /// 空记录。下载器用它作为构造基线，避免每个调用点重复写 7 个 `None`。
    pub fn absent() -> Self {
        Self {
            status: PartitionStatus::Absent,
            source_sha256: None,
            source_url: None,
            parquet_path: None,
            parquet_bytes: None,
            fetched_at: None,
            restated_at: None,
        }
    }
}

/// 数据缺口。跨越缺口的回测会凭空发明不可能的成交，所以必须显式记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub kind: DatasetKind,
    pub symbol: String,
    /// 缺口起始（含）。
    pub from: DateTime<Utc>,
    /// 缺口结束（不含）。
    pub to: DateTime<Utc>,
    pub note: String,
}

/// 台账里的一条分区记录。
///
/// 分区键被**展平**进记录本身，而不是作为 map 的键。原因是 JSON 的对象键
/// 必须是字符串，用结构体作键会让 `serde_json` 直接报 "key must be a string"。
/// 展平后的形式也更易读、更易 diff：
///
/// ```json
/// { "kind": "agg_trades", "symbol": "ETHUSDC", "year": 2026, "month": 8,
///   "status": { "status": "finalized", ... }, "source_sha256": "..." }
/// ```
// 注意：这里刻意**不用** `#[serde(flatten)]` 把 `entry` 铺平。
//
// `flatten` 会让 serde 生成一个内部用 `f64`/`f32` 做值缓冲的反序列化器，
// 从而触发 clippy.toml 的浮点禁用规则——那是派生宏的误报，本结构体的数值
// 全是 `i64` 定点或整数。可以 `#[allow]` 豁免，但豁免无法可靠地覆盖宏展开，
// 而且会留下一个"为什么这里允许浮点"的长期疑问。
//
// 代价只是 JSON 多一层嵌套，换来 lint 规则保持无豁免地生效。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRecord {
    pub kind: DatasetKind,
    pub symbol: String,
    pub year: i32,
    pub month: u32,
    pub entry: PartitionEntry,
}

impl ManifestRecord {
    pub fn key(&self) -> PartitionKey {
        PartitionKey {
            kind: self.kind,
            symbol: self.symbol.clone(),
            year: self.year,
            month: self.month,
        }
    }
}

/// 整个下载台账。
///
/// 内存中用 `BTreeMap` 做按键查找（下载器的主要操作），落盘时展平为数组
/// （见 `ManifestRecord`）。两者在 `load`/`save` 之间转换。
#[derive(Clone, Debug)]
pub struct Manifest {
    pub schema_version: u32,
    /// BTreeMap 保证迭代顺序稳定，便于生成可 diff 的输出。
    pub partitions: BTreeMap<PartitionKey, PartitionEntry>,
    pub gaps: Vec<Gap>,
    /// 台账最后更新时间。
    pub updated_at: DateTime<Utc>,
}

/// 落盘形式。与 `Manifest` 互转。
#[derive(Serialize, Deserialize)]
struct ManifestFile {
    schema_version: u32,
    partitions: Vec<ManifestRecord>,
    gaps: Vec<Gap>,
    updated_at: DateTime<Utc>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            partitions: BTreeMap::new(),
            gaps: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

impl Manifest {
    /// 从磁盘加载。文件不存在时返回空台账。
    ///
    /// # 解析顺序
    ///
    /// 先单独读出 `schema_version`，**再**解析结构。这个顺序很重要：未来
    /// 格式变更时，用户应该看到"版本 N 不受支持"，而不是"文件损坏"——
    /// 后者会让人误以为磁盘出问题，前者才指向真实原因（程序版本不匹配）。
    ///
    /// 未知版本报错而非重置。静默重置会让"已下载"的记忆丢失，导致重复下载
    /// 12 GB 数据，甚至误判数据可用。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("读取台账失败: {}", path.display()))?;

        // 第一阶段：只取版本号。
        let probe: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("台账损坏，拒绝重置: {}", path.display()))?;
        let version = probe
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .with_context(|| {
                format!(
                    "台账缺少 schema_version 字段，拒绝按当前格式解析: {}",
                    path.display()
                )
            })? as u32;

        if version != CURRENT_SCHEMA_VERSION {
            bail!(
                "台账版本 {version} 不受当前程序支持（期望 {CURRENT_SCHEMA_VERSION}），\
                 拒绝启动。请勿手工修改 schema_version；如需降级，请从归档重新下载。"
            );
        }

        // 第二阶段：版本确认后才按当前结构解析。
        let file: ManifestFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("台账损坏，拒绝重置: {}", path.display()))?;

        let mut manifest = Manifest {
            schema_version: file.schema_version,
            partitions: file
                .partitions
                .into_iter()
                .map(|r| {
                    let key = r.key();
                    let entry = r.entry;
                    (key, entry)
                })
                .collect(),
            gaps: file.gaps,
            updated_at: file.updated_at,
        };
        manifest.migrate()?;
        Ok(manifest)
    }

    /// 迁移梯子。当前只有 v1，暂无迁移分支。
    ///
    /// 这里刻意保持"没有分支就报错"的形状：新增版本时必须在 `match` 里
    /// 显式处理，而不是靠 serde default 静默填补。
    fn migrate(&mut self) -> Result<()> {
        match self.schema_version {
            CURRENT_SCHEMA_VERSION => Ok(()),
            other => bail!("没有从版本 {other} 到 {CURRENT_SCHEMA_VERSION} 的迁移路径"),
        }
    }

    /// 原子保存：写临时文件 → fsync → rename → fsync 目录。
    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.updated_at = Utc::now();
        let tmp = path.with_extension("json.tmp");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建台账目录失败: {}", parent.display()))?;
        }

        let file = ManifestFile {
            schema_version: self.schema_version,
            partitions: self
                .partitions
                .iter()
                .map(|(k, e)| ManifestRecord {
                    kind: k.kind,
                    symbol: k.symbol.clone(),
                    year: k.year,
                    month: k.month,
                    entry: e.clone(),
                })
                .collect(),
            gaps: self.gaps.clone(),
            updated_at: self.updated_at,
        };
        let bytes = serde_json::to_vec_pretty(&file).context("序列化台账失败")?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("创建临时台账失败: {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("替换台账失败: {}", path.display()))?;

        // fsync 父目录，确保 rename 本身落盘。缺少这一步时断电可能丢失整个台账。
        if let Some(parent) = path.parent() {
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    }

    pub fn entry(&self, key: &PartitionKey) -> Option<&PartitionEntry> {
        self.partitions.get(key)
    }

    /// 记录一个分区结果。
    pub fn record(&mut self, key: PartitionKey, entry: PartitionEntry) {
        self.partitions.insert(key, entry);
    }

    /// 所有需要处理的月份：给定标的目标区间内，尚未完成的分区。
    pub fn pending(
        &self,
        kind: DatasetKind,
        symbol: &str,
        from: (i32, u32),
        to: (i32, u32),
    ) -> Vec<PartitionKey> {
        let mut out = Vec::new();
        for (year, month) in months_between(from, to) {
            let key = PartitionKey {
                kind,
                symbol: symbol.to_string(),
                year,
                month,
            };
            let done = self
                .partitions
                .get(&key)
                .is_some_and(|e| e.status.is_done());
            if !done {
                out.push(key);
            }
        }
        out
    }

    /// 某数据集某标的已完成的覆盖区间。
    pub fn coverage(&self, kind: DatasetKind, symbol: &str) -> Vec<(i32, u32)> {
        self.partitions
            .iter()
            .filter(|(k, e)| k.kind == kind && k.symbol == symbol && e.status.is_usable())
            .map(|(k, _)| (k.year, k.month))
            .collect()
    }

    /// 与某数据集某标的相关的缺口。
    pub fn gaps_for(&self, kind: DatasetKind, symbol: &str) -> Vec<&Gap> {
        self.gaps
            .iter()
            .filter(|g| g.kind == kind && g.symbol == symbol)
            .collect()
    }

    pub fn add_gap(&mut self, gap: Gap) {
        // 避免同一缺口的重复记录
        if !self.gaps.iter().any(|g| {
            g.kind == gap.kind && g.symbol == gap.symbol && g.from == gap.from && g.to == gap.to
        }) {
            self.gaps.push(gap);
        }
    }
}

/// 枚举 `from` 到 `to` 之间的所有 `(年, 月)`，含两端。
pub fn months_between(from: (i32, u32), to: (i32, u32)) -> Vec<(i32, u32)> {
    let mut out = Vec::new();
    let (mut y, mut m) = from;
    // 上限保护：避免参数写错时无限循环。
    for _ in 0..600 {
        if (y, m) > to {
            break;
        }
        out.push((y, m));
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    out
}

/// 分区对应的归档 URL 与文件名。
pub fn archive_url_and_name(base: &str, key: &PartitionKey) -> (String, String) {
    let tag = key.kind.file_tag();
    let dir = key.kind.archive_dir();
    let symbol = &key.symbol;
    let name = format!("{symbol}-{tag}-{}-{:02}.zip", key.year, key.month);
    let url = format!("{base}/{dir}/{symbol}/{name}");
    (url, name)
}

/// 文件名用的日期辅助（日粒度扩展时用）。
pub fn date_tag(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn months_between_includes_both_ends() {
        let m = months_between((2024, 1), (2024, 3));
        assert_eq!(m, vec![(2024, 1), (2024, 2), (2024, 3)]);
    }

    #[test]
    fn months_between_crosses_year_boundary() {
        let m = months_between((2025, 11), (2026, 2));
        assert_eq!(m, vec![(2025, 11), (2025, 12), (2026, 1), (2026, 2)]);
    }

    #[test]
    fn months_between_single_month() {
        assert_eq!(months_between((2026, 8), (2026, 8)), vec![(2026, 8)]);
    }

    /// 参数写反时应返回空而不是崩溃或无限循环。
    #[test]
    fn months_between_reversed_range_is_empty() {
        assert!(months_between((2026, 8), (2024, 1)).is_empty());
    }

    #[test]
    fn archive_url_matches_binance_layout() {
        let key = PartitionKey {
            kind: DatasetKind::AggTrades,
            symbol: "ETHUSDC".into(),
            year: 2026,
            month: 8,
        };
        let (url, name) =
            archive_url_and_name("https://data.binance.vision/data/futures/um/monthly", &key);
        assert_eq!(name, "ETHUSDC-aggTrades-2026-08.zip");
        assert_eq!(
            url,
            "https://data.binance.vision/data/futures/um/monthly/aggTrades/ETHUSDC/ETHUSDC-aggTrades-2026-08.zip"
        );
    }

    #[test]
    fn klines_archive_url_uses_interval_segment() {
        let key = PartitionKey {
            kind: DatasetKind::Klines1m,
            symbol: "BTCUSDC".into(),
            year: 2024,
            month: 1,
        };
        let (url, name) =
            archive_url_and_name("https://data.binance.vision/data/futures/um/monthly", &key);
        assert_eq!(name, "BTCUSDC-1m-2024-01.zip");
        assert!(
            url.contains("/klines/BTCUSDC/BTCUSDC-1m-2024-01.zip"),
            "{url}"
        );
    }

    /// 行数期望是判断"数据是否完整"的依据，必须准确。
    #[test]
    fn expected_rows_reflect_real_granularity() {
        assert_eq!(DatasetKind::Klines1m.expected_rows_per_day(), Some(1440));
        assert_eq!(DatasetKind::FundingRate.expected_rows_per_day(), Some(3));
        assert_eq!(
            DatasetKind::AggTrades.expected_rows_per_day(),
            None,
            "逐笔成交没有固定行数，不能假装有"
        );
    }

    #[test]
    fn unknown_schema_version_is_rejected_not_reset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        // 写入一个未来版本
        std::fs::write(
            &path,
            br#"{"schema_version":99,"partitions":{},"gaps":[],"updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let err = Manifest::load(&path).unwrap_err().to_string();
        assert!(err.contains("99"), "错误信息应包含实际版本号：{err}");
        assert!(err.contains("拒绝"), "必须拒绝而非静默重置：{err}");
    }

    #[test]
    fn corrupt_manifest_is_rejected_not_reset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let err = Manifest::load(&path).unwrap_err().to_string();
        assert!(err.contains("损坏"), "{err}");
    }

    #[test]
    fn missing_manifest_yields_empty_default() {
        let dir = tempdir().unwrap();
        let m = Manifest::load(&dir.path().join("nope.json")).unwrap();
        assert!(m.partitions.is_empty());
        assert_eq!(m.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub/manifest.json");
        let mut m = Manifest::default();
        m.record(
            PartitionKey {
                kind: DatasetKind::AggTrades,
                symbol: "ETHUSDC".into(),
                year: 2026,
                month: 8,
            },
            PartitionEntry {
                status: PartitionStatus::Finalized {
                    row_count: 14_094_668,
                    min_ts: Utc::now(),
                    max_ts: Utc::now(),
                },
                source_sha256: Some("abc123".into()),
                source_url: Some("https://example/x.zip".into()),
                parquet_path: Some(PathBuf::from("lake/agg_trades/x.parquet")),
                parquet_bytes: Some(1234),
                fetched_at: Some(Utc::now()),
                restated_at: None,
            },
        );
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.partitions.len(), 1);
        let e = loaded
            .entry(&PartitionKey {
                kind: DatasetKind::AggTrades,
                symbol: "ETHUSDC".into(),
                year: 2026,
                month: 8,
            })
            .unwrap();
        assert!(e.status.is_usable());
        assert_eq!(e.source_sha256.as_deref(), Some("abc123"));
    }

    /// 已完成的分区不能再出现在待办列表里，否则会重复下载 12 GB。
    #[test]
    fn pending_skips_finalized_and_absent_from_archive() {
        let mut m = Manifest::default();
        let done = PartitionKey {
            kind: DatasetKind::Klines1m,
            symbol: "ETHUSDC".into(),
            year: 2024,
            month: 1,
        };
        m.record(
            done.clone(),
            PartitionEntry {
                status: PartitionStatus::Finalized {
                    row_count: 1440,
                    min_ts: Utc::now(),
                    max_ts: Utc::now(),
                },
                ..PartitionEntry::absent()
            },
        );
        let gone = PartitionKey {
            kind: DatasetKind::Klines1m,
            symbol: "ETHUSDC".into(),
            year: 2024,
            month: 2,
        };
        m.record(
            gone,
            PartitionEntry {
                status: PartitionStatus::NotInArchive,
                ..PartitionEntry::absent()
            },
        );

        let pending = m.pending(DatasetKind::Klines1m, "ETHUSDC", (2024, 1), (2024, 3));
        assert_eq!(pending.len(), 1, "只应剩 2024-03");
        assert_eq!(pending[0].month, 3);
    }

    /// `Suspicious` 与 `Failed` 都必须重新处理——不能把可疑数据当可用。
    #[test]
    fn suspicious_and_failed_are_retried() {
        let mut m = Manifest::default();
        for (month, status) in [
            (
                1u32,
                PartitionStatus::Suspicious {
                    row_count: 1400,
                    expected: 1440,
                    note: "缺 40 根".into(),
                },
            ),
            (
                2,
                PartitionStatus::Failed {
                    error: "网络中断".into(),
                    attempts: 3,
                },
            ),
        ] {
            m.record(
                PartitionKey {
                    kind: DatasetKind::Klines1m,
                    symbol: "ETHUSDC".into(),
                    year: 2024,
                    month,
                },
                PartitionEntry {
                    status,
                    ..PartitionEntry::absent()
                },
            );
        }

        let pending = m.pending(DatasetKind::Klines1m, "ETHUSDC", (2024, 1), (2024, 2));
        assert_eq!(pending.len(), 2, "可疑与失败都必须重试");
        assert!(
            !m.entry(&PartitionKey {
                kind: DatasetKind::Klines1m,
                symbol: "ETHUSDC".into(),
                year: 2024,
                month: 1,
            })
            .unwrap()
            .status
            .is_usable()
        );
    }

    #[test]
    fn duplicate_gaps_are_not_recorded_twice() {
        let mut m = Manifest::default();
        let g = Gap {
            kind: DatasetKind::AggTrades,
            symbol: "ETHUSDC".into(),
            from: Utc::now(),
            to: Utc::now(),
            note: "数据内部空洞".into(),
        };
        m.add_gap(g.clone());
        m.add_gap(g);
        assert_eq!(m.gaps.len(), 1);
    }

    #[test]
    fn coverage_lists_only_usable_partitions() {
        let mut m = Manifest::default();
        m.record(
            PartitionKey {
                kind: DatasetKind::Klines1m,
                symbol: "ETHUSDC".into(),
                year: 2024,
                month: 1,
            },
            PartitionEntry {
                status: PartitionStatus::Finalized {
                    row_count: 1440,
                    min_ts: Utc::now(),
                    max_ts: Utc::now(),
                },
                ..PartitionEntry::absent()
            },
        );
        m.record(
            PartitionKey {
                kind: DatasetKind::Klines1m,
                symbol: "ETHUSDC".into(),
                year: 2024,
                month: 2,
            },
            PartitionEntry {
                status: PartitionStatus::Suspicious {
                    row_count: 1,
                    expected: 1440,
                    note: "x".into(),
                },
                ..PartitionEntry::absent()
            },
        );
        assert_eq!(
            m.coverage(DatasetKind::Klines1m, "ETHUSDC"),
            vec![(2024, 1)]
        );
    }
}
