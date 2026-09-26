//! 币安归档的 S3 列举索引。
//!
//! # 为什么需要它
//!
//! 台账（[`crate::manifest`]）记录的是"我们下载过什么"，但不知道"归档里
//! 实际有什么"。请求区间超出归档实际覆盖范围时，下载器会对每个月份都收到
//! 404，既浪费请求，也污染台账（`Failed` 记录堆积）。这个模块直接向 S3
//! 列举归档目录，得到某标的某数据集的**真实**月份范围，用来裁剪下载计划。
//!
//! # 为什么用 S3 主机而不是 `data.binance.vision`
//!
//! `data.binance.vision` 对象列举查询只返回一段人类可读的 HTML（浏览器
//! 目录索引），没有机器可读的结构。实测确认同样的查询参数发到 S3 桶的
//! 原生主机（`s3-ap-northeast-1.amazonaws.com`，托管 `data.binance.vision`
//! 这个桶）会返回标准的 `ListBucketResult` XML。
//!
//! **这不是币安 fapi**：不计任何 IP 权重，也没有等价的推送/WS
//! 接口——币安没有提供"列出归档里有哪些月份"的实时通道，这是静态对象
//! 存储的目录信息，只能靠列举 API 拿到。data crate 不持有任何交易所凭据，
//! 这个主机也**绝不能**被加入 `exchange` 的签名请求白名单：它与账户、
//! 下单、行情推送完全无关，只是历史归档的静态文件列表。

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};

use crate::manifest::{DatasetKind, Manifest, PartitionKey, archive_prefix};

/// S3 列举 API 的主机。**不提供环境变量覆盖**——这是列举归档用的固定
/// 基础设施地址，不是可配置的业务参数，配置错了会静默列不到任何东西。
pub const ARCHIVE_LIST_BASE: &str = "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision";

/// 归档路径前缀的根：`{ARCHIVE_PREFIX_ROOT}{archive_prefix(kind, symbol)}`
/// 就是某数据集某标的的完整 S3 前缀。
pub const ARCHIVE_PREFIX_ROOT: &str = "data/futures/um/monthly/";

/// 单次列举请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 网络重试的退避序列：1s / 2s / 4s，最多 3 次重试（合计最多 4 次请求）。
const RETRY_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// 分页上限。超过这个页数还没翻完，说明响应异常（例如一直缺
/// `NextMarker` 却还标 truncated），拒绝无限翻页。
const MAX_PAGES: usize = 5;

/// 翻页之间的固定间隔，避免无间隔连续请求同一主机。
const PAGE_INTERVAL: Duration = Duration::from_millis(300);

/// 构造某数据集某标的的列举 URL。
///
/// `marker` 用于翻页（S3 V1 列举 API 语义：从上一页最后一个 key 之后继续）。
/// `prefix` 与 `marker` 都做百分号编码——`/` 是路径分隔符，保留不转义，
/// 其它任何非"未保留字符"都编码，避免遇到含特殊字符的 marker 时拼出
/// 语义错误的查询串。
pub fn list_url(kind: DatasetKind, symbol: &str, marker: Option<&str>) -> String {
    let prefix = format!("{ARCHIVE_PREFIX_ROOT}{}", archive_prefix(kind, symbol));
    let mut url = format!(
        "{ARCHIVE_LIST_BASE}?prefix={}&delimiter=/",
        percent_encode_query(&prefix)
    );
    if let Some(marker) = marker {
        url.push_str("&marker=");
        url.push_str(&percent_encode_query(marker));
    }
    url
}

/// 最小百分号编码：保留 ASCII 字母、数字、`-` `_` `.` `~`（RFC 3986
/// 未保留字符）与 `/`（S3 前缀本身就是路径，不应被转义），其它字节一律
/// 编码为 `%XX`（大写十六进制）。
///
/// 项目里没有引入 `url` crate作为 `data` 的依赖，而这里只需要处理
/// S3 前缀与 marker 这两种已知形状的字符串，所以手写这个最小实现，
/// 不必为此新增依赖。
fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// 一页 `ListBucketResult` 的解析结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPage {
    /// 本页全部 `<Key>`（含 `.zip` 与 `.zip.CHECKSUM`）。
    pub keys: Vec<String>,
    /// 是否还有下一页。
    pub truncated: bool,
    /// 下一页的 marker。`truncated` 为真但响应没给 `NextMarker` 时
    /// （S3 V1 列举 API 的老语义），用本页最后一个 key 顶替。
    pub next_marker: Option<String>,
}

/// 解析 S3 `ListBucketResult` XML。
///
/// 手写最小标签切分，而不是引入完整 XML 解析库——响应结构固定且简单
/// （标签不嵌套同名标签，属性也不携带需要解析的信息），完整解析器对这个
/// 场景是过度设计。
pub fn parse_list_bucket(xml: &str) -> Result<ListPage, String> {
    let keys: Vec<String> = extract_all_tag_contents(xml, "Key")
        .into_iter()
        .map(|s| decode_xml_entities(&s))
        .collect();

    let truncated = match extract_tag_content(xml, "IsTruncated") {
        Some(v) => match v.trim() {
            "true" => true,
            "false" => false,
            other => return Err(format!("无法解析 IsTruncated 的值：{other}")),
        },
        None => return Err("响应缺少 IsTruncated 字段，无法判断是否需要翻页".to_string()),
    };

    let explicit_next_marker = extract_tag_content(xml, "NextMarker")
        .map(|s| decode_xml_entities(&s))
        .filter(|s| !s.is_empty());

    // S3 V1 列举语义：truncated 但没给 NextMarker 时，用本页最后一个 key
    // 顶替（严格来说 marker 应该是 key 本身，不是 key 的某个后继值）。
    let next_marker = explicit_next_marker.or_else(|| {
        if truncated {
            keys.last().cloned()
        } else {
            None
        }
    });

    Ok(ListPage {
        keys,
        truncated,
        next_marker,
    })
}

/// 提取第一个 `<tag>...</tag>` 之间的内容。
fn extract_tag_content(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// 提取全部 `<tag>...</tag>` 之间的内容，按出现顺序。
fn extract_all_tag_contents(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start_rel) = rest.find(&open) {
        let after_open = &rest[start_rel + open.len()..];
        let Some(end_rel) = after_open.find(&close) else {
            break;
        };
        out.push(after_open[..end_rel].to_string());
        rest = &after_open[end_rel + close.len()..];
    }
    out
}

/// 解码 XML 里出现的实体引用（`&amp;` `&lt;` `&gt;` `&apos;` `&quot;`
/// 与 `&#NN;` / `&#xHH;` 数字引用）。未知实体原样保留，不静默丢字符。
fn decode_xml_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut entity = String::new();
        let mut closed = false;
        while let Some(&c2) = chars.peek() {
            if c2 == ';' {
                chars.next();
                closed = true;
                break;
            }
            // 实体名不会很长，超过阈值说明这不是一个合法实体（避免把整段
            // 字符串误吞进去）。
            if entity.len() > 10 {
                break;
            }
            entity.push(c2);
            chars.next();
        }
        if !closed {
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        match entity.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "apos" => out.push('\''),
            "quot" => out.push('"'),
            other if other.starts_with("#x") || other.starts_with("#X") => {
                if let Some(cp) = u32::from_str_radix(&other[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    out.push(cp);
                } else {
                    out.push('&');
                    out.push_str(&entity);
                    out.push(';');
                }
            }
            other if other.starts_with('#') => {
                if let Some(cp) = other[1..].parse::<u32>().ok().and_then(char::from_u32) {
                    out.push(cp);
                } else {
                    out.push('&');
                    out.push_str(&entity);
                    out.push(';');
                }
            }
            _ => {
                out.push('&');
                out.push_str(&entity);
                out.push(';');
            }
        }
    }
    out
}

/// 从一批 S3 key 里筛出真正属于 `kind`/`symbol` 的月度归档，解析出
/// `(年, 月)`。
///
/// 只匹配严格形如 `{SYMBOL}-{file_tag}-YYYY-MM.zip` 的**文件名**（取 key
/// 最后一段），据此自动排除：
/// - `.zip.CHECKSUM`（后缀不是 `.zip`）；
/// - 其它交易对（例如 `ETHUSDCX` 混入 `ETHUSDC` 的列举结果时，前缀不严格
///   相等会被拒绝）；
/// - 目录条目等任何不匹配这个精确形状的 key。
pub fn months_from_keys(keys: &[String], kind: DatasetKind, symbol: &str) -> BTreeSet<(i32, u32)> {
    let name_prefix = format!("{symbol}-{}-", kind.file_tag());
    let mut out = BTreeSet::new();
    for key in keys {
        let name = key.rsplit('/').next().unwrap_or(key.as_str());
        let Some(rest) = name.strip_prefix(&name_prefix) else {
            continue;
        };
        let Some(ym) = rest.strip_suffix(".zip") else {
            continue;
        };
        let mut parts = ym.split('-');
        let (Some(y), Some(m), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if y.len() != 4 || m.len() != 2 {
            continue;
        }
        let (Ok(year), Ok(month)) = (y.parse::<i32>(), m.parse::<u32>()) else {
            continue;
        };
        if !(1..=12).contains(&month) {
            continue;
        }
        out.insert((year, month));
    }
    out
}

/// 某数据集某标的在归档里实际存在的月份集合。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveMonths {
    pub months: BTreeSet<(i32, u32)>,
}

impl ArchiveMonths {
    pub fn earliest(&self) -> Option<(i32, u32)> {
        self.months.iter().next().copied()
    }

    pub fn latest(&self) -> Option<(i32, u32)> {
        self.months.iter().next_back().copied()
    }
}

/// 列举某数据集某标的的全部归档月份（自动翻页）。
///
/// # 重试与超时
///
/// 单次请求超时 15 秒；只对超时、连接错误、5xx、429 重试，最多 3 次，
/// 退避 1s/2s/4s——其它任何 4xx（含 404）都是"这个请求本身就有问题"，
/// 重试无意义，直接报错。
///
/// # 分页
///
/// 最多翻 5 页，页间隔 300ms（避免无间隔连续请求）。ETHUSDC 四类数据集
/// 实测都是单页（未截断），5 页上限是防御性的：真的翻到这么多页说明
/// 响应异常（例如一直缺 `NextMarker` 却仍标记 truncated），此时应该报错
/// 而不是无限翻下去。
pub async fn fetch_archive_months(
    client: &reqwest::Client,
    kind: DatasetKind,
    symbol: &str,
) -> Result<ArchiveMonths> {
    let mut months = BTreeSet::new();
    let mut marker: Option<String> = None;

    for page in 1..=MAX_PAGES {
        let url = list_url(kind, symbol, marker.as_deref());
        let xml = fetch_list_page_with_retry(client, &url).await?;
        let list_page =
            parse_list_bucket(&xml).map_err(|e| anyhow::anyhow!("解析归档列举结果失败：{e}"))?;
        months.extend(months_from_keys(&list_page.keys, kind, symbol));

        if !list_page.truncated {
            return Ok(ArchiveMonths { months });
        }
        let Some(next) = list_page.next_marker else {
            bail!("列举 {symbol} 的 {kind:?} 归档时响应被截断但没有可用的翻页 marker");
        };
        marker = Some(next);

        if page == MAX_PAGES {
            bail!("列举 {symbol} 的 {kind:?} 归档超过 {MAX_PAGES} 页仍未结束，拒绝继续翻页");
        }
        tokio::time::sleep(PAGE_INTERVAL).await;
    }

    // 循环体在最后一页时必然已经 return 或 bail，这里不可达。
    unreachable!("分页循环必须在结束前 return 或 bail")
}

/// 带重试地取一页列举响应体。只对超时、连接错误、5xx、429 重试。
async fn fetch_list_page_with_retry(client: &reqwest::Client, url: &str) -> Result<String> {
    for (attempt, backoff) in std::iter::once(None)
        .chain(RETRY_BACKOFFS.iter().map(Some))
        .enumerate()
    {
        if let Some(backoff) = backoff {
            tokio::time::sleep(*backoff).await;
        }
        let is_last_attempt = attempt == RETRY_BACKOFFS.len();

        match client.get(url).timeout(REQUEST_TIMEOUT).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp.text().await.context("读取归档列举响应体失败");
                }
                let retryable =
                    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                if !retryable || is_last_attempt {
                    bail!("列举归档失败：HTTP {status}（{url}）");
                }
            }
            Err(e) => {
                let retryable = e.is_timeout() || e.is_connect();
                if !retryable || is_last_attempt {
                    bail!("列举归档请求出错：{e}（{url}）");
                }
            }
        }
    }
    unreachable!("重试循环必须在结束前 return 或 bail")
}

/// [`plan_work`] 的输出：真正要下载的分区、因超出归档范围被裁剪的说明、
/// 索引本身是否可用。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkPlan {
    pub work: Vec<PartitionKey>,
    /// 中文说明，描述哪些请求区间被裁剪、为什么。供 CLI/API 直接展示。
    pub clipped: Vec<String>,
    /// 是否所有 (kind, symbol) 组合都拿到了列举结果。任一组合的
    /// `index` 返回 `None`（列举失败/未启用）就是 `false`——调用方应据此
    /// 提示"这次的计划范围可能不准"。
    pub index_available: bool,
}

fn dataset_label(kind: DatasetKind) -> &'static str {
    match kind {
        DatasetKind::Klines1m => "K 线",
        DatasetKind::AggTrades => "逐笔成交",
        DatasetKind::MarkPriceKlines1m => "标记价 K 线",
        DatasetKind::FundingRate => "资金费率",
    }
}

fn fmt_ym((y, m): (i32, u32)) -> String {
    format!("{y:04}-{m:02}")
}

fn prev_month_of((y, m): (i32, u32)) -> (i32, u32) {
    if m == 1 { (y - 1, 12) } else { (y, m - 1) }
}

fn next_month_of((y, m): (i32, u32)) -> (i32, u32) {
    if m == 12 { (y + 1, 1) } else { (y, m + 1) }
}

/// 根据归档实际覆盖范围（`index` 提供）裁剪下载计划。纯函数，不做任何
/// IO——`index` 由调用方注入，通常是"提前列举好、按 (kind, symbol) 查表"
/// 的闭包。
///
/// # 裁剪规则
///
/// - `index` 返回 `Some(months)` 且非空：待办 = 请求区间 ∩
///   `[earliest, latest]` 里 `manifest.pending` 判定未完成的月份；早于
///   `earliest` 或晚于 `latest` 的部分不进入 `work`，也不落台账，只在
///   `clipped` 里各留一条中文说明。
/// - `index` 返回 `Some(months)` 但为空（归档里没有这个组合，例如标的
///   不存在）：该组合 `work` 为空，`clipped` 说明"归档中没有 XX 的 YY"。
/// - `index` 返回 `None`（列举失败或未启用）：把 `to` 裁到 `today` 的
///   上一个月（如果请求的 `to` 更晚），在 `clipped` 里说明，并把
///   `index_available` 置为 `false`。
///
/// 区间中间缺的月份（不在两端裁剪范围内）照常进入 `work`，让下载器如实
/// 记录 404——这不是本函数要处理的裁剪对象。
pub fn plan_work(
    manifest: &Manifest,
    kinds: &[DatasetKind],
    symbols: &[String],
    from: (i32, u32),
    to: (i32, u32),
    index: &dyn Fn(DatasetKind, &str) -> Option<ArchiveMonths>,
    today: NaiveDate,
) -> WorkPlan {
    let mut work = Vec::new();
    let mut clipped = Vec::new();
    let mut index_available = true;

    for &kind in kinds {
        for symbol in symbols {
            let label = dataset_label(kind);
            match index(kind, symbol.as_str()) {
                Some(months) => {
                    let Some(earliest) = months.earliest() else {
                        clipped.push(format!("归档中没有 {symbol} 的 {label}"));
                        continue;
                    };
                    // `earliest` 存在时 `latest` 必然也存在（同一个非空集合）。
                    let latest = months.latest().unwrap_or(earliest);

                    let mut effective_from = from;
                    let mut effective_to = to;

                    if from < earliest {
                        let skipped_to = if to < earliest {
                            to
                        } else {
                            prev_month_of(earliest)
                        };
                        clipped.push(format!(
                            "{symbol} {label}：{} 至 {} 早于归档最早月份 {}，已跳过",
                            fmt_ym(from),
                            fmt_ym(skipped_to),
                            fmt_ym(earliest)
                        ));
                        effective_from = earliest;
                    }

                    if to > latest {
                        let skipped_from = if from > latest {
                            from
                        } else {
                            next_month_of(latest)
                        };
                        if skipped_from <= to {
                            clipped.push(format!(
                                "{symbol} {label}：{} 至 {} 晚于归档最新月份 {}，已跳过",
                                fmt_ym(skipped_from),
                                fmt_ym(to),
                                fmt_ym(latest)
                            ));
                        }
                        effective_to = latest;
                    }

                    work.extend(manifest.pending(kind, symbol, effective_from, effective_to));
                }
                None => {
                    index_available = false;
                    let today_prev = prev_month_of((today.year(), today.month()));
                    let mut effective_to = to;
                    if to > today_prev {
                        clipped.push(format!(
                            "{symbol} {label}：归档索引不可用，目标月份已裁剪至 {}（{} 的上一个月）",
                            fmt_ym(today_prev),
                            fmt_ym((today.year(), today.month()))
                        ));
                        effective_to = today_prev;
                    }
                    work.extend(manifest.pending(kind, symbol, from, effective_to));
                }
            }
        }
    }

    WorkPlan {
        work,
        clipped,
        index_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    // ---------------- list_url ----------------

    #[test]
    fn base_host_is_the_s3_endpoint_not_the_cdn() {
        let url = reqwest::Url::parse(ARCHIVE_LIST_BASE).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("s3-ap-northeast-1.amazonaws.com"));
    }

    #[test]
    fn list_url_matches_binance_archive_layout_for_all_kinds() {
        let cases: [(DatasetKind, &str); 4] = [
            (
                DatasetKind::Klines1m,
                "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/futures/um/monthly/klines/ETHUSDC/1m/&delimiter=/",
            ),
            (
                DatasetKind::MarkPriceKlines1m,
                "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/futures/um/monthly/markPriceKlines/ETHUSDC/1m/&delimiter=/",
            ),
            (
                DatasetKind::AggTrades,
                "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/futures/um/monthly/aggTrades/ETHUSDC/&delimiter=/",
            ),
            (
                DatasetKind::FundingRate,
                "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/futures/um/monthly/fundingRate/ETHUSDC/&delimiter=/",
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(list_url(kind, "ETHUSDC", None), expected, "{kind:?}");
        }
    }

    #[test]
    fn list_url_with_marker_appends_and_encodes_it() {
        let url = list_url(
            DatasetKind::AggTrades,
            "ETHUSDC",
            Some("data/futures/um/monthly/aggTrades/ETHUSDC/ETHUSDC-aggTrades-2024-06.zip"),
        );
        assert_eq!(
            url,
            "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=data/futures/um/monthly/aggTrades/ETHUSDC/&delimiter=/&marker=data/futures/um/monthly/aggTrades/ETHUSDC/ETHUSDC-aggTrades-2024-06.zip"
        );
    }

    /// marker 中出现的查询保留字符必须被转义，否则会拼出语义错误的查询串。
    #[test]
    fn percent_encode_query_escapes_reserved_characters_but_keeps_slash() {
        assert_eq!(percent_encode_query("a/b"), "a/b");
        assert_eq!(percent_encode_query("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode_query("a b"), "a%20b");
    }

    // ---------------- parse_list_bucket ----------------

    #[test]
    fn parse_list_bucket_empty_result() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>data.binance.vision</Name>
  <Prefix>data/futures/um/monthly/klines/NOPE/1m/</Prefix>
  <Marker></Marker>
  <MaxKeys>1000</MaxKeys>
  <Delimiter>/</Delimiter>
  <IsTruncated>false</IsTruncated>
</ListBucketResult>"#;
        let page = parse_list_bucket(xml).unwrap();
        assert!(page.keys.is_empty());
        assert!(!page.truncated);
        assert_eq!(page.next_marker, None);
    }

    #[test]
    fn parse_list_bucket_normal_result_ignores_unrelated_tags() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>data.binance.vision</Name>
  <Prefix>data/futures/um/monthly/klines/ETHUSDC/1m/</Prefix>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip</Key>
    <LastModified>2024-02-01T00:00:00.000Z</LastModified>
    <ETag>"abc"</ETag>
    <Size>123</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip.CHECKSUM</Key>
    <LastModified>2024-02-01T00:00:00.000Z</LastModified>
    <ETag>"def"</ETag>
    <Size>64</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes>
    <Prefix>data/futures/um/monthly/klines/ETHUSDC/1m/</Prefix>
  </CommonPrefixes>
</ListBucketResult>"#;
        let page = parse_list_bucket(xml).unwrap();
        assert_eq!(
            page.keys,
            vec![
                "data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip".to_string(),
                "data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip.CHECKSUM"
                    .to_string(),
            ]
        );
        assert!(!page.truncated);
        assert_eq!(page.next_marker, None);
    }

    #[test]
    fn parse_list_bucket_truncated_with_next_marker() {
        let xml = r#"<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <NextMarker>data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-06.zip</NextMarker>
  <Contents><Key>data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-05.zip</Key></Contents>
</ListBucketResult>"#;
        let page = parse_list_bucket(xml).unwrap();
        assert!(page.truncated);
        assert_eq!(
            page.next_marker,
            Some("data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-06.zip".to_string())
        );
    }

    /// S3 V1 列举语义：截断但没给 `NextMarker` 时，用本页最后一个 key
    /// 顶替，否则翻页会卡住。
    #[test]
    fn parse_list_bucket_truncated_without_next_marker_falls_back_to_last_key() {
        let xml = r#"<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <Contents><Key>a/1.zip</Key></Contents>
  <Contents><Key>a/2.zip</Key></Contents>
</ListBucketResult>"#;
        let page = parse_list_bucket(xml).unwrap();
        assert!(page.truncated);
        assert_eq!(page.next_marker, Some("a/2.zip".to_string()));
    }

    #[test]
    fn parse_list_bucket_decodes_xml_entities_in_keys() {
        let xml = r#"<ListBucketResult>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>a/b&amp;c/ETHUSDC-1m-2024-01.zip</Key></Contents>
</ListBucketResult>"#;
        let page = parse_list_bucket(xml).unwrap();
        assert_eq!(page.keys, vec!["a/b&c/ETHUSDC-1m-2024-01.zip".to_string()]);
    }

    #[test]
    fn parse_list_bucket_missing_is_truncated_is_an_error() {
        let xml = "<ListBucketResult></ListBucketResult>";
        let err = parse_list_bucket(xml).unwrap_err();
        assert!(err.contains("IsTruncated"), "{err}");
    }

    // ---------------- months_from_keys ----------------

    #[test]
    fn months_from_keys_ignores_checksum_files() {
        let keys = vec![
            "data/.../ETHUSDC-1m-2024-01.zip".to_string(),
            "data/.../ETHUSDC-1m-2024-01.zip.CHECKSUM".to_string(),
        ];
        let months = months_from_keys(&keys, DatasetKind::Klines1m, "ETHUSDC");
        assert_eq!(months, BTreeSet::from([(2024, 1)]));
    }

    /// 别的交易对（例如列举结果里意外混入 `ETHUSDCX`）绝不能被计入
    /// `ETHUSDC` 的月份集合——前缀比较必须是精确匹配，不是 `starts_with`
    /// 意义上的子串。
    #[test]
    fn months_from_keys_ignores_other_symbols() {
        let keys = vec![
            "data/.../ETHUSDC-1m-2024-01.zip".to_string(),
            "data/.../ETHUSDCX-1m-2024-02.zip".to_string(),
            "data/.../BTCUSDC-1m-2024-03.zip".to_string(),
        ];
        let months = months_from_keys(&keys, DatasetKind::Klines1m, "ETHUSDC");
        assert_eq!(months, BTreeSet::from([(2024, 1)]));
    }

    #[test]
    fn months_from_keys_parses_kline_1m_prefix_correctly() {
        let keys = vec![
            "data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip".to_string(),
            "data/futures/um/monthly/klines/ETHUSDC/1m/ETHUSDC-1m-2024-02.zip".to_string(),
        ];
        let months = months_from_keys(&keys, DatasetKind::Klines1m, "ETHUSDC");
        assert_eq!(months, BTreeSet::from([(2024, 1), (2024, 2)]));
    }

    #[test]
    fn archive_months_earliest_and_latest() {
        let m = ArchiveMonths {
            months: BTreeSet::from([(2024, 3), (2024, 1), (2025, 12)]),
        };
        assert_eq!(m.earliest(), Some((2024, 1)));
        assert_eq!(m.latest(), Some((2025, 12)));
        assert_eq!(ArchiveMonths::default().earliest(), None);
    }

    // ---------------- plan_work ----------------

    fn months_of(pairs: &[(i32, u32)]) -> ArchiveMonths {
        ArchiveMonths {
            months: pairs.iter().copied().collect(),
        }
    }

    fn today_fixed() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 26).unwrap()
    }

    #[test]
    fn plan_work_clips_request_to_archive_range_with_two_notes() {
        let manifest = Manifest::default();
        let kinds = [DatasetKind::Klines1m];
        let symbols = vec!["ETHUSDC".to_string()];
        let archive_range: Vec<(i32, u32)> = crate::manifest::months_between((2024, 1), (2026, 8));
        let months = months_of(&archive_range);

        let plan = plan_work(
            &manifest,
            &kinds,
            &symbols,
            (2020, 1),
            (2026, 9),
            &|kind, symbol| {
                assert_eq!(kind, DatasetKind::Klines1m);
                assert_eq!(symbol, "ETHUSDC");
                Some(months.clone())
            },
            today_fixed(),
        );

        assert!(plan.index_available);
        assert_eq!(
            plan.clipped,
            vec![
                "ETHUSDC K 线：2020-01 至 2023-12 早于归档最早月份 2024-01，已跳过".to_string(),
                "ETHUSDC K 线：2026-09 至 2026-09 晚于归档最新月份 2026-08，已跳过".to_string(),
            ]
        );
        assert_eq!(plan.work.len(), archive_range.len());
        assert_eq!(
            plan.work.first().map(|k| (k.year, k.month)),
            Some((2024, 1))
        );
        assert_eq!(plan.work.last().map(|k| (k.year, k.month)), Some((2026, 8)));
    }

    #[test]
    fn plan_work_excludes_already_finalized_months() {
        let mut manifest = Manifest::default();
        manifest.record(
            PartitionKey {
                kind: DatasetKind::Klines1m,
                symbol: "ETHUSDC".into(),
                year: 2024,
                month: 2,
            },
            crate::manifest::PartitionEntry {
                status: crate::manifest::PartitionStatus::Finalized {
                    row_count: 1440 * 29,
                    min_ts: Utc::now(),
                    max_ts: Utc::now(),
                },
                ..crate::manifest::PartitionEntry::absent()
            },
        );

        let kinds = [DatasetKind::Klines1m];
        let symbols = vec!["ETHUSDC".to_string()];
        let months = months_of(&[(2024, 1), (2024, 2), (2024, 3)]);

        let plan = plan_work(
            &manifest,
            &kinds,
            &symbols,
            (2024, 1),
            (2024, 3),
            &|_, _| Some(months.clone()),
            today_fixed(),
        );

        let got: Vec<(i32, u32)> = plan.work.iter().map(|k| (k.year, k.month)).collect();
        assert_eq!(
            got,
            vec![(2024, 1), (2024, 3)],
            "2024-02 已完成，不应重复出现"
        );
        assert!(
            plan.clipped.is_empty(),
            "请求区间完全在归档范围内，不应有裁剪说明"
        );
    }

    #[test]
    fn plan_work_clips_to_last_month_when_index_unavailable() {
        let manifest = Manifest::default();
        let kinds = [DatasetKind::Klines1m];
        let symbols = vec!["ETHUSDC".to_string()];

        let plan = plan_work(
            &manifest,
            &kinds,
            &symbols,
            (2024, 1),
            (2026, 12),
            &|_, _| None,
            today_fixed(),
        );

        assert!(!plan.index_available);
        assert_eq!(plan.clipped.len(), 1);
        assert!(plan.clipped[0].contains("2026-08"), "{:?}", plan.clipped);
        assert_eq!(
            plan.work.last().map(|k| (k.year, k.month)),
            Some((2026, 8)),
            "无索引时应把 to 裁到 today 的上一个月"
        );
    }

    #[test]
    fn plan_work_is_empty_when_archive_has_no_such_symbol() {
        let manifest = Manifest::default();
        let kinds = [DatasetKind::Klines1m];
        let symbols = vec!["ETHUSDC".to_string()];

        let plan = plan_work(
            &manifest,
            &kinds,
            &symbols,
            (2024, 1),
            (2024, 3),
            &|_, _| Some(ArchiveMonths::default()),
            today_fixed(),
        );

        assert!(plan.work.is_empty());
        assert_eq!(plan.clipped, vec!["归档中没有 ETHUSDC 的 K 线".to_string()]);
    }
}
