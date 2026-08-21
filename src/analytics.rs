use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result};
use chrono::{Datelike as _, Local, TimeZone as _, Timelike as _};
use rusqlite::{Connection, params};
use url::Url;

use crate::events::{ActionExecutionSummary, InterceptionProtocol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartRange {
    Day,
    Week,
    Month,
}

impl ChartRange {
    pub fn bucket_seconds(self) -> i64 {
        match self {
            Self::Day => 60 * 60,
            Self::Week => 24 * 60 * 60,
            Self::Month => 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DashboardStats {
    pub series: Vec<SeriesPoint>,
}

#[derive(Debug, Clone)]
pub struct SeriesPoint {
    pub label: String,
    pub count: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ActionEventGroup {
    pub occurred_at: i64,
    pub rule_id: String,
    pub host: String,
    pub path: String,
    pub protocol: InterceptionProtocol,
    pub action_results: Vec<ActionExecutionSummary>,
    pub hit_count: u64,
    pub action_count: u64,
    pub detail_redacted: bool,
    oldest_at: i64,
}

impl ActionEventGroup {
    pub fn preview(summary: ActionExecutionSummary) -> Self {
        let now = Local::now().timestamp();
        Self {
            occurred_at: now,
            rule_id: "Action 预览".into(),
            host: "应用内预览".into(),
            path: String::new(),
            protocol: InterceptionProtocol::Unknown,
            action_results: vec![summary],
            hit_count: 1,
            action_count: 1,
            detail_redacted: false,
            oldest_at: now,
        }
    }
}

pub struct Analytics {
    path: PathBuf,
}

impl Analytics {
    pub fn new(path: PathBuf) -> Result<Self> {
        let analytics = Self { path };
        let connection = analytics.connect()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS interception_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                occurred_at INTEGER NOT NULL,
                rule_id TEXT NOT NULL,
                host TEXT NOT NULL,
                path TEXT NOT NULL,
                action_count INTEGER NOT NULL,
                detail_redacted INTEGER NOT NULL DEFAULT 0,
                protocol TEXT NOT NULL DEFAULT 'unknown',
                action_results_json TEXT NOT NULL DEFAULT '[]'
            );
            CREATE INDEX IF NOT EXISTS idx_interception_time
                ON interception_events(occurred_at);
            CREATE INDEX IF NOT EXISTS idx_interception_rule
                ON interception_events(rule_id);",
        )?;
        let _ = connection.execute(
            "ALTER TABLE interception_events
             ADD COLUMN detail_redacted INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE interception_events
             ADD COLUMN protocol TEXT NOT NULL DEFAULT 'unknown'",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE interception_events
             ADD COLUMN action_results_json TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        Ok(analytics)
    }

    fn connect(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("无法打开统计数据库 {}", self.path.display()))?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(connection)
    }

    pub fn record(
        &self,
        rule_id: &str,
        request: &str,
        protocol: InterceptionProtocol,
        action_results: &[ActionExecutionSummary],
        detailed_logging: bool,
    ) -> Result<()> {
        let parsed = Url::parse(request).ok();
        let host = parsed
            .as_ref()
            .and_then(Url::host_str)
            .unwrap_or("unknown")
            .to_string();
        let path = parsed
            .as_ref()
            .map(|url| url.path().to_string())
            .unwrap_or_else(|| "/".into());
        let (host, path, detail_redacted) = if detailed_logging {
            (host, path, 0)
        } else {
            ("已关闭详细日志".to_string(), String::new(), 1)
        };
        let action_results_json = serde_json::to_string(action_results)?;
        let protocol = protocol_value(protocol);
        self.connect()?.execute(
            "INSERT INTO interception_events
             (occurred_at, rule_id, host, path, action_count, detail_redacted,
              protocol, action_results_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                Local::now().timestamp(),
                rule_id,
                host,
                path,
                action_results.len(),
                detail_redacted,
                protocol,
                action_results_json,
            ],
        )?;
        Ok(())
    }

    pub fn recent_action_groups(
        &self,
        limit: usize,
        window_seconds: i64,
        retention_days: u32,
    ) -> Result<Vec<ActionEventGroup>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let cutoff = Local::now().timestamp() - i64::from(retention_days) * 86_400;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT occurred_at, rule_id, host, path, action_count, detail_redacted,
                    protocol, action_results_json
             FROM interception_events
             WHERE occurred_at >= ?1
             ORDER BY occurred_at DESC, id DESC
             LIMIT 2000",
        )?;
        let rows = statement.query_map([cutoff], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, i64>(5)? != 0,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;

        let mut groups: Vec<ActionEventGroup> = Vec::new();
        for row in rows {
            let (
                occurred_at,
                rule_id,
                host,
                path,
                action_count,
                detail_redacted,
                protocol,
                action_results_json,
            ) = row?;
            let protocol = parse_protocol(&protocol);
            let action_results =
                serde_json::from_str::<Vec<ActionExecutionSummary>>(&action_results_json)
                    .unwrap_or_default();
            let signature = serde_json::to_string(&action_results).unwrap_or_default();
            let matching = groups.iter_mut().find(|group| {
                group.rule_id == rule_id
                    && group.host == host
                    && group.protocol == protocol
                    && serde_json::to_string(&group.action_results).unwrap_or_default() == signature
                    && group.oldest_at - occurred_at <= window_seconds
            });
            if let Some(group) = matching {
                group.oldest_at = occurred_at;
                group.hit_count += 1;
                group.action_count += action_count;
            } else if groups.len() < limit {
                groups.push(ActionEventGroup {
                    occurred_at,
                    rule_id,
                    host,
                    path,
                    protocol,
                    action_results,
                    hit_count: 1,
                    action_count,
                    detail_redacted,
                    oldest_at: occurred_at,
                });
            }
        }
        Ok(groups)
    }

    pub fn dashboard(&self, range: ChartRange) -> Result<DashboardStats> {
        let connection = self.connect()?;
        let now = Local::now();
        let bucket_seconds = range.bucket_seconds();
        let bucket_count = match range {
            ChartRange::Day => 24,
            ChartRange::Week => 7,
            ChartRange::Month => 30,
        };
        let end_bucket = now.timestamp() / bucket_seconds * bucket_seconds;
        let first_bucket = end_bucket - (bucket_count as i64 - 1) * bucket_seconds;
        let mut counts = vec![0u64; bucket_count];
        let mut statement = connection.prepare(
            "SELECT (occurred_at / ?1) * ?1 AS bucket, COUNT(*)
             FROM interception_events
             WHERE occurred_at >= ?2
             GROUP BY bucket ORDER BY bucket",
        )?;
        let rows = statement.query_map(params![bucket_seconds, first_bucket], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (bucket, count) = row?;
            let index = ((bucket - first_bucket) / bucket_seconds) as usize;
            if let Some(slot) = counts.get_mut(index) {
                *slot = count;
            }
        }
        let series = counts
            .into_iter()
            .enumerate()
            .map(|(index, count)| {
                let timestamp = first_bucket + index as i64 * bucket_seconds;
                let moment = Local.timestamp_opt(timestamp, 0).single().unwrap_or(now);
                SeriesPoint {
                    label: match range {
                        ChartRange::Day => format!("{:02}:00", moment.hour()),
                        ChartRange::Week | ChartRange::Month => {
                            format!("{:02}/{:02}", moment.month(), moment.day())
                        }
                    },
                    count,
                }
            })
            .collect();

        Ok(DashboardStats { series })
    }

    pub fn maintain(&self, detailed_days: u32, aggregate_days: u32) -> Result<()> {
        let now = Local::now().timestamp();
        let detail_cutoff = now - i64::from(detailed_days) * 86_400;
        let aggregate_cutoff = now - i64::from(aggregate_days) * 86_400;
        self.connect()?.execute(
            "UPDATE interception_events
             SET host = '已按保留策略清理', path = '', detail_redacted = 1
             WHERE occurred_at < ?1 AND detail_redacted = 0",
            params![detail_cutoff],
        )?;
        self.connect()?.execute(
            "DELETE FROM interception_events WHERE occurred_at < ?1",
            params![aggregate_cutoff],
        )?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM interception_events", [])?;
        Ok(())
    }

    pub fn export_csv(&self, path: &Path) -> Result<()> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT occurred_at, rule_id, host, path, action_count, detail_redacted,
                    protocol, action_results_json
             FROM interception_events ORDER BY occurred_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut csv = String::from(
            "occurred_at,rule_id,host,path,action_count,detail_redacted,protocol,action_results_json\n",
        );
        for row in rows {
            let (
                occurred_at,
                rule_id,
                host,
                path,
                action_count,
                redacted,
                protocol,
                action_results_json,
            ) = row?;
            csv.push_str(&format!(
                "{occurred_at},{},{},{},{action_count},{redacted},{},{}\n",
                csv_cell(&rule_id),
                csv_cell(&host),
                csv_cell(&path),
                csv_cell(&protocol),
                csv_cell(&action_results_json),
            ));
        }
        std::fs::write(path, csv).with_context(|| format!("无法导出统计到 {}", path.display()))?;
        Ok(())
    }
}

fn protocol_value(protocol: InterceptionProtocol) -> &'static str {
    match protocol {
        InterceptionProtocol::Http => "http",
        InterceptionProtocol::Https => "https",
        InterceptionProtocol::Unknown => "unknown",
    }
}

fn parse_protocol(value: &str) -> InterceptionProtocol {
    match value {
        "http" => InterceptionProtocol::Http,
        "https" => InterceptionProtocol::Https,
        _ => InterceptionProtocol::Unknown,
    }
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ActionExecutionStatus, ActionSurface};

    fn result(id: &str, status: ActionExecutionStatus) -> ActionExecutionSummary {
        ActionExecutionSummary {
            action_id: id.into(),
            kind: "popup_image".into(),
            status,
            surface: ActionSurface::InAppCard,
            error: None,
        }
    }

    #[test]
    fn records_dashboard_and_exports_without_query_strings() {
        let temp = tempfile::tempdir().unwrap();
        let analytics = Analytics::new(temp.path().join("stats.sqlite3")).unwrap();
        analytics
            .record(
                "rule-a",
                "http://example.test/blocked?secret=1",
                InterceptionProtocol::Http,
                &[result("image", ActionExecutionStatus::Succeeded)],
                true,
            )
            .unwrap();
        let stats = analytics.dashboard(ChartRange::Day).unwrap();
        assert_eq!(stats.series.iter().map(|point| point.count).sum::<u64>(), 1);

        let export = temp.path().join("stats.csv");
        analytics.export_csv(&export).unwrap();
        let csv = std::fs::read_to_string(export).unwrap();
        assert!(csv.contains("rule-a"));
        assert!(!csv.contains("secret=1"));
        assert!(!csv.contains("error"));
    }

    #[test]
    fn persisted_action_json_excludes_error_and_local_details() {
        let temp = tempfile::tempdir().unwrap();
        let analytics = Analytics::new(temp.path().join("stats.sqlite3")).unwrap();
        let mut summary = result("html", ActionExecutionStatus::Failed);
        summary.error = Some("无法读取 C:\\private\\blocked.html".into());
        analytics
            .record(
                "rule-a",
                "http://example.test/",
                InterceptionProtocol::Http,
                &[summary],
                true,
            )
            .unwrap();
        let connection = analytics.connect().unwrap();
        let json: String = connection
            .query_row(
                "SELECT action_results_json FROM interception_events LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(json.contains("action_id"));
        assert!(json.contains("surface"));
        assert!(!json.contains("error"));
        assert!(!json.contains("private"));
    }

    #[test]
    fn disabled_detail_logging_keeps_aggregate_only() {
        let temp = tempfile::tempdir().unwrap();
        let analytics = Analytics::new(temp.path().join("stats.sqlite3")).unwrap();
        analytics
            .record(
                "rule-private",
                "http://private.test/path",
                InterceptionProtocol::Http,
                &[result("image", ActionExecutionStatus::Succeeded)],
                false,
            )
            .unwrap();
        let stats = analytics.dashboard(ChartRange::Day).unwrap();
        assert_eq!(stats.series.iter().map(|point| point.count).sum::<u64>(), 1);
        let export = temp.path().join("stats.csv");
        analytics.export_csv(&export).unwrap();
        let csv = std::fs::read_to_string(export).unwrap();
        assert!(!csv.contains("private.test"));
        assert!(!csv.contains("/path"));
    }

    #[test]
    fn rolling_window_groups_consecutive_hits_but_splits_gaps_and_signatures() {
        let temp = tempfile::tempdir().unwrap();
        let analytics = Analytics::new(temp.path().join("stats.sqlite3")).unwrap();
        let success = result("image", ActionExecutionStatus::Succeeded);
        for _ in 0..3 {
            analytics
                .record(
                    "rule-a",
                    "http://example.test/path",
                    InterceptionProtocol::Http,
                    std::slice::from_ref(&success),
                    true,
                )
                .unwrap();
        }
        analytics
            .record(
                "rule-a",
                "http://example.test/path",
                InterceptionProtocol::Http,
                &[result("image", ActionExecutionStatus::Failed)],
                true,
            )
            .unwrap();
        let connection = analytics.connect().unwrap();
        let now = Local::now().timestamp();
        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 1",
                [now - 50],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 2",
                [now - 25],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 3",
                [now],
            )
            .unwrap();

        let groups = analytics.recent_action_groups(20, 30, 7).unwrap();
        assert_eq!(groups.len(), 2, "结果签名变化必须拆组");
        assert!(groups.iter().any(|group| group.hit_count == 3));

        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 1",
                [now - 70],
            )
            .unwrap();
        let groups = analytics.recent_action_groups(20, 30, 7).unwrap();
        assert_eq!(groups.len(), 3, "连续命中间隔超过 30 秒必须拆组");
    }

    #[test]
    fn migrates_legacy_database_without_losing_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE interception_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    occurred_at INTEGER NOT NULL,
                    rule_id TEXT NOT NULL,
                    host TEXT NOT NULL,
                    path TEXT NOT NULL,
                    action_count INTEGER NOT NULL,
                    detail_redacted INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO interception_events
                 (occurred_at, rule_id, host, path, action_count, detail_redacted)
                 VALUES (?1, 'legacy-rule', 'legacy.test', '/', 0, 0)",
                [Local::now().timestamp()],
            )
            .unwrap();
        drop(connection);

        let analytics = Analytics::new(path).unwrap();
        let groups = analytics.recent_action_groups(20, 30, 7).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].protocol, InterceptionProtocol::Unknown);
        assert!(groups[0].action_results.is_empty());
    }

    #[test]
    fn maintenance_redacts_details_after_seven_days_and_keeps_aggregate_count() {
        let temp = tempfile::tempdir().unwrap();
        let analytics = Analytics::new(temp.path().join("stats.sqlite3")).unwrap();
        let summary = result("image", ActionExecutionStatus::Succeeded);
        for host in ["eight-days.test", "expired.test"] {
            analytics
                .record(
                    "rule-a",
                    &format!("http://{host}/private"),
                    InterceptionProtocol::Http,
                    std::slice::from_ref(&summary),
                    true,
                )
                .unwrap();
        }
        let now = Local::now().timestamp();
        let connection = analytics.connect().unwrap();
        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 1",
                [now - 8 * 86_400],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE interception_events SET occurred_at = ?1 WHERE id = 2",
                [now - 91 * 86_400],
            )
            .unwrap();
        drop(connection);

        analytics.maintain(7, 90).unwrap();
        let connection = analytics.connect().unwrap();
        let rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM interception_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        let (host, path, redacted): (String, String, i64) = connection
            .query_row(
                "SELECT host, path, detail_redacted FROM interception_events LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(host, "已按保留策略清理");
        assert!(path.is_empty());
        assert_eq!(redacted, 1);
    }
}
