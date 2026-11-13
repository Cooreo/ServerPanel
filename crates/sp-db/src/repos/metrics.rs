//! `metric_samples` + `player_samples`: the chart history.
//!
//! Written by the monitor service every 30 s and pruned on a schedule. Kept in
//! SQLite rather than a TSDB on purpose: one file, no service, and a `LIMIT` query
//! is all the UI needs.

use crate::db::Db;
use crate::error::DbResult;
use sp_core::ids::ServerId;
use sp_core::model::{now_ms, PlayerSample};
use sqlx::Row;

pub struct Metrics<'a> {
    pub(crate) db: &'a Db,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub t_ms: i64,
    pub cpu_percent: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub tps: Option<f64>,
    pub mspt: Option<f64>,
    pub players_online: u32,
}

impl<'a> Metrics<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub async fn record(&self, server_id: ServerId, s: Sample) -> DbResult<()> {
        let _ = sqlx::query(
            "INSERT INTO metric_samples (server_id, t_ms, cpu_percent, mem_used, mem_total, tps, mspt, players_online)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(server_id, t_ms) DO UPDATE SET cpu_percent = excluded.cpu_percent,
                 mem_used = excluded.mem_used, mem_total = excluded.mem_total, tps = excluded.tps,
                 mspt = excluded.mspt, players_online = excluded.players_online",
        )
        .bind(server_id.get())
        .bind(if s.t_ms == 0 { now_ms() } else { s.t_ms })
        .bind(s.cpu_percent)
        .bind(s.mem_used as i64)
        .bind(s.mem_total as i64)
        .bind(s.tps)
        .bind(s.mspt)
        .bind(i64::from(s.players_online))
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Oldest-first series for the charts, downsampled to at most `max_points` by
    /// taking the *worst* value per bucket (a chart should never hide a spike).
    pub async fn series(
        &self,
        server_id: ServerId,
        since_ms: i64,
        max_points: usize,
    ) -> DbResult<Vec<Sample>> {
        let rows = sqlx::query(
            "SELECT t_ms, cpu_percent, mem_used, mem_total, tps, mspt, players_online
             FROM metric_samples WHERE server_id = ? AND t_ms >= ? ORDER BY t_ms ASC",
        )
        .bind(server_id.get())
        .bind(since_ms)
        .fetch_all(self.db.pool())
        .await?;
        let mut all = Vec::with_capacity(rows.len());
        for row in &rows {
            all.push(Sample {
                t_ms: row.try_get("t_ms")?,
                cpu_percent: row.try_get::<f64, _>("cpu_percent").unwrap_or(0.0),
                mem_used: row.try_get::<i64, _>("mem_used").unwrap_or(0).max(0) as u64,
                mem_total: row.try_get::<i64, _>("mem_total").unwrap_or(0).max(0) as u64,
                tps: row.try_get::<Option<f64>, _>("tps").ok().flatten(),
                mspt: row.try_get::<Option<f64>, _>("mspt").ok().flatten(),
                players_online: row.try_get::<i64, _>("players_online").unwrap_or(0).max(0) as u32,
            });
        }
        if all.len() <= max_points.max(2) || max_points == 0 {
            return Ok(all);
        }
        let bucket = all.len().div_ceil(max_points);
        let mut out = Vec::with_capacity(all.len() / bucket + 1);
        for chunk in all.chunks(bucket) {
            let mut peak = chunk[0].clone();
            for s in chunk {
                if s.cpu_percent > peak.cpu_percent {
                    peak.cpu_percent = s.cpu_percent;
                }
                if s.mem_used > peak.mem_used {
                    peak.mem_used = s.mem_used;
                }
                if s.players_online > peak.players_online {
                    peak.players_online = s.players_online;
                }
                // TPS: keep the *lowest* — that is the one that indicates lag.
                match (peak.tps, s.tps) {
                    (Some(p), Some(v)) if v < p => peak.tps = Some(v),
                    (None, Some(v)) => peak.tps = Some(v),
                    _ => {}
                }
                if s.mspt > peak.mspt {
                    peak.mspt = s.mspt;
                }
            }
            out.push(peak);
        }
        Ok(out)
    }

    pub async fn record_players(&self, server_id: ServerId, s: PlayerSample) -> DbResult<()> {
        let _ = sqlx::query(
            "INSERT INTO player_samples (server_id, t_ms, online, max) VALUES (?, ?, ?, ?)
             ON CONFLICT(server_id, t_ms) DO UPDATE SET online = excluded.online, max = excluded.max",
        )
        .bind(server_id.get())
        .bind(if s.t_ms == 0 { now_ms() } else { s.t_ms })
        .bind(i64::from(s.online))
        .bind(i64::from(s.max))
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn player_history(
        &self,
        server_id: ServerId,
        since_ms: i64,
    ) -> DbResult<Vec<PlayerSample>> {
        let rows = sqlx::query(
            "SELECT t_ms, online, max FROM player_samples WHERE server_id = ? AND t_ms >= ? ORDER BY t_ms ASC",
        )
        .bind(server_id.get())
        .bind(since_ms)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(PlayerSample {
                    t_ms: row.try_get("t_ms")?,
                    online: row.try_get::<i64, _>("online").unwrap_or(0).max(0) as u32,
                    max: row.try_get::<i64, _>("max").unwrap_or(0).max(0) as u32,
                })
            })
            .collect::<DbResult<Vec<_>>>()?)
    }

    pub async fn prune(&self, keep_days: u32) -> DbResult<u64> {
        let cutoff = now_ms() - i64::from(keep_days.max(1)) * 86_400_000;
        let a = sqlx::query("DELETE FROM metric_samples WHERE t_ms < ?")
            .bind(cutoff)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        let b = sqlx::query("DELETE FROM player_samples WHERE t_ms < ?")
            .bind(cutoff)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        Ok(a + b)
    }

    pub async fn clear(&self, server_id: ServerId) -> DbResult<()> {
        for table in ["metric_samples", "player_samples"] {
            let _ = sqlx::query(&format!("DELETE FROM {table} WHERE server_id = ?"))
                .bind(server_id.get())
                .execute(self.db.pool())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DbPath};
    use sp_core::model::ResourceStats;

    #[tokio::test]
    async fn records_queries_and_downsamples_to_peaks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::connect(&DbPath::file(dir.path().join("p.db")))
            .await
            .unwrap();
        let sid = ServerId::new(1);
        for i in 0..20 {
            db.metrics()
                .record(
                    sid,
                    Sample {
                        t_ms: 1_000 + i * 30_000,
                        cpu_percent: if i == 7 { 99.0 } else { 10.0 },
                        mem_used: 1_000,
                        mem_total: 4_000,
                        tps: Some(if i == 7 { 12.0 } else { 20.0 }),
                        mspt: Some(20.0),
                        players_online: 3,
                    },
                )
                .await
                .unwrap();
        }
        let full = db.metrics().series(sid, 0, 0).await.unwrap();
        assert_eq!(full.len(), 20);
        let shrunk = db.metrics().series(sid, 0, 4).await.unwrap();
        assert!(shrunk.len() <= 5 && shrunk.len() >= 3, "{:?}", shrunk.len());
        assert!(
            shrunk.iter().any(|s| s.cpu_percent > 98.0),
            "the spike survives downsampling"
        );
        assert!(
            shrunk.iter().any(|s| s.tps == Some(12.0)),
            "the TPS dip survives too"
        );
        // out-of-range server / window
        assert!(db
            .metrics()
            .series(ServerId::new(99), 0, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .metrics()
            .series(sid, i64::MAX, 0)
            .await
            .unwrap()
            .is_empty());

        db.metrics()
            .record_players(
                sid,
                PlayerSample {
                    t_ms: 1_000,
                    online: 5,
                    max: 20,
                },
            )
            .await
            .unwrap();
        let hist = db.metrics().player_history(sid, 0).await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].online, 5);
        assert_eq!(hist[0].max, 20);

        assert!(db.metrics().prune(0).await.unwrap() >= 21);
        assert!(db.metrics().series(sid, 0, 0).await.unwrap().is_empty());
        let _ = ResourceStats::default();
    }
}
