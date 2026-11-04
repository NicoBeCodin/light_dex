use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::Path;
pub struct Store(pub Connection);
impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let c = Connection::open(path)?;
        c.busy_timeout(std::time::Duration::from_secs(2))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA auto_vacuum=INCREMENTAL;
   CREATE TABLE IF NOT EXISTS claims(address TEXT PRIMARY KEY,digest TEXT NOT NULL,day INTEGER NOT NULL,client TEXT NOT NULL);
   CREATE INDEX IF NOT EXISTS claims_day ON claims(day,client);
   CREATE TABLE IF NOT EXISTS submissions(digest TEXT PRIMARY KEY,address TEXT NOT NULL,raw TEXT NOT NULL,created INTEGER NOT NULL,status TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS receipts(digest TEXT PRIMARY KEY,sequence INTEGER NOT NULL,address TEXT NOT NULL,body TEXT NOT NULL,created INTEGER NOT NULL,latency INTEGER);
   CREATE INDEX IF NOT EXISTS receipt_address ON receipts(address,created);
   CREATE TABLE IF NOT EXISTS blocks(sequence INTEGER PRIMARY KEY,body TEXT NOT NULL,created INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS trades(market INTEGER,sequence INTEGER,tx INTEGER,fill INTEGER,body TEXT NOT NULL,created INTEGER NOT NULL,PRIMARY KEY(market,sequence,tx,fill));
   CREATE INDEX IF NOT EXISTS receipt_created ON receipts(created);
   CREATE INDEX IF NOT EXISTS submission_pending ON submissions(address,status,created);
   CREATE INDEX IF NOT EXISTS submission_created ON submissions(created);
   CREATE INDEX IF NOT EXISTS trade_created ON trades(created);
   CREATE INDEX IF NOT EXISTS trade_market_time ON trades(market,created);
   CREATE INDEX IF NOT EXISTS block_created ON blocks(created);
   CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        Ok(Self(c))
    }
    pub fn claim(
        &mut self,
        address: &str,
        digest: &str,
        client: &str,
        now: i64,
    ) -> Result<(), String> {
        let tx = self.0.transaction().map_err(|_| "quota database busy")?;
        let day = now / 86400;
        let old: Option<String> = tx
            .query_row(
                "SELECT digest FROM claims WHERE address=?",
                [address],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| "quota unavailable")?;
        if let Some(old) = old {
            return if old == digest {
                Ok(())
            } else {
                Err("This address has already reserved its faucet claim".into())
            };
        }
        let global: i64 = tx
            .query_row("SELECT count(*) FROM claims WHERE day=?", [day], |r| {
                r.get(0)
            })
            .map_err(|_| "quota unavailable")?;
        let personal: i64 = tx
            .query_row(
                "SELECT count(*) FROM claims WHERE day=? AND client=?",
                params![day, client],
                |r| r.get(0),
            )
            .map_err(|_| "quota unavailable")?;
        if global >= 100 || personal >= 2 {
            return Err("Daily faucet allowance reached; try again tomorrow".into());
        }
        tx.execute(
            "INSERT INTO claims VALUES(?,?,?,?)",
            params![address, digest, day, client],
        )
        .map_err(|_| "quota unavailable")?;
        tx.commit().map_err(|_| "quota unavailable".to_string())
    }
    pub fn submission(
        &self,
        digest: &str,
        address: &str,
        raw: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let outstanding:i64=self.0.query_row("SELECT count(*) FROM submissions WHERE address=? AND status IN ('submitting','admitted') AND created>?",params![address,now-300],|r|r.get(0))?;
        let known: bool = self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE digest=?)",
            [digest],
            |r| r.get(0),
        )?;
        if outstanding >= 8 && !known {
            return Ok(false);
        }
        self.0.execute(
            "INSERT OR IGNORE INTO submissions VALUES(?,?,?,?,'submitting')",
            params![digest, address, raw, now],
        )?;
        Ok(true)
    }
    pub fn receipt(
        &self,
        digest: &str,
        body: &Value,
        now: i64,
        latency: Option<i64>,
    ) -> rusqlite::Result<()> {
        self.0.execute("INSERT INTO receipts VALUES(?,?,?,?,?,?) ON CONFLICT(digest) DO UPDATE SET sequence=excluded.sequence,body=CASE WHEN json_type(excluded.body,'$.success') IN ('true','false') OR json_type(receipts.body,'$.success') NOT IN ('true','false') THEN excluded.body ELSE receipts.body END,latency=coalesce(receipts.latency,excluded.latency)",params![digest,body["block_sequence"].as_u64().unwrap_or(0),body["account"].as_str().unwrap_or(""),body.to_string(),now,latency])?;
        self.0.execute(
            "UPDATE submissions SET status='committed' WHERE digest=?",
            [digest],
        )?;
        Ok(())
    }
    pub fn receipt_batch(
        &self,
        rows: &[(&str, &Value, Option<i64>)],
        now: i64,
    ) -> rusqlite::Result<()> {
        let transaction = self.0.unchecked_transaction()?;
        for (digest, body, latency) in rows {
            self.receipt(digest, body, now, *latency)?;
        }
        transaction.commit()
    }
    pub fn lookup(&self, digest: &str) -> rusqlite::Result<Value> {
        let receipt: Option<String> = self
            .0
            .query_row("SELECT body FROM receipts WHERE digest=?", [digest], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(body) = receipt {
            let signed: Option<String> = self
                .0
                .query_row(
                    "SELECT raw FROM submissions WHERE digest=?",
                    [digest],
                    |r| r.get(0),
                )
                .optional()?;
            return Ok(
                serde_json::json!({"status":"committed","receipt":serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),"signed_transaction":signed.and_then(|raw|serde_json::from_str::<Value>(&raw).ok())}),
            );
        }
        let status: Option<String> = self
            .0
            .query_row(
                "SELECT status FROM submissions WHERE digest=?",
                [digest],
                |r| r.get(0),
            )
            .optional()?;
        Ok(serde_json::json!({"status":status.unwrap_or_else(||"unknown".into())}))
    }
    pub fn prune(&self, now: i64) -> rusqlite::Result<()> {
        let cutoff = now - 7 * 86400;
        for table in ["submissions", "receipts", "blocks", "trades"] {
            self.0
                .execute(&format!("DELETE FROM {table} WHERE created<?"), [cutoff])?;
        }
        self.0
            .execute("DELETE FROM claims WHERE day<?", [now / 86400 - 2])?;
        self.0
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum(1000);")
    }
}
use rusqlite::OptionalExtension;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_batch_rolls_back_and_preserves_known_outcomes() {
        let db = Store::open(Path::new(":memory:")).unwrap();
        let good = serde_json::json!({"block_sequence":1,"account":"a","success":true});
        let bad = serde_json::json!({"block_sequence":u64::MAX,"account":"a","success":true});
        assert!(
            db.receipt_batch(&[("first", &good, None), ("bad", &bad, None)], 1)
                .is_err()
        );
        assert_eq!(db.lookup("first").unwrap()["status"], "unknown");
        db.receipt_batch(&[("first", &good, Some(100))], 1).unwrap();
        let unknown = serde_json::json!({"block_sequence":1,"account":"a","success":null});
        db.receipt_batch(&[("first", &unknown, None)], 2).unwrap();
        assert_eq!(db.lookup("first").unwrap()["receipt"]["success"], true);
    }
    #[test]
    fn quota_is_idempotent_and_persistent() {
        let p = std::env::temp_dir().join(format!("ldex-quota-{}.db", rand::random::<u64>()));
        {
            let mut db = Store::open(&p).unwrap();
            assert!(db.claim("a", "x", "ip", 100).is_ok());
            assert!(db.claim("a", "x", "ip", 100).is_ok());
            assert!(db.claim("a", "y", "ip", 100).is_err());
            assert!(db.claim("b", "z", "ip", 100).is_ok());
        }
        let mut db = Store::open(&p).unwrap();
        assert!(db.claim("c", "k", "ip", 100).is_err());
        assert!(db.claim("c", "k", "ip", 86500).is_ok());
        drop(db);
        let _ = std::fs::remove_file(p);
    }
}
