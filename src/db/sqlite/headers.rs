use std::collections::BTreeMap;
use std::fs;
use std::ops::{Bound, RangeBounds};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use bitcoin::block::{Header, Version};
use bitcoin::{BlockHash, CompactTarget, Network, TxMerkleNode};
use rusqlite::{params, params_from_iter, Connection, Result};
use tokio::sync::Mutex;

use crate::db::error::{SqlHeaderStoreError, SqlInitializationError};
use crate::db::traits::HeaderStore;
use crate::prelude::FutureResult;

use super::{DATA_DIR, DEFAULT_CWD};

const FILE_NAME: &str = "headers.db";
// Labels for the schema table
const SCHEMA_TABLE_NAME: &str = "header_schema_versions";
const SCHEMA_COLUMN: &str = "schema_key";
const VERSION_COLUMN: &str = "version";
const SCHEMA_KEY: &str = "current_version";
// Update this in the case of schema changes
const SCHEMA_VERSION: u8 = 0;
// Always execute this query and adjust the schema with migrations
const INITIAL_HEADER_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS headers (
    height INTEGER PRIMARY KEY,
    block_hash TEXT NOT NULL,
    version INTEGER NOT NULL,
    prev_hash TEXT NOT NULL,
    merkle_root TEXT NOT NULL,
    time INTEGER NOT NULL,
    bits INTEGER NOT NULL,
    nonce INTEGER NOT NULL
) STRICT";

const LOAD_QUERY_SELECT_PREFIX: &str = "SELECT * FROM headers ";
const LOAD_QUERY_ORDERBY_SUFFIX: &str = "ORDER BY height";

/// Header storage implementation with SQL Lite.
#[derive(Debug)]
pub struct SqliteHeaderDb {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteHeaderDb {
    /// Create a new [`SqliteHeaderDb`] with an optional file path. If no path is provided,
    /// the file will be stored in a `data` subdirectory where the program is ran.
    pub fn new(network: Network, path: Option<PathBuf>) -> Result<Self, SqlInitializationError> {
        let mut path = path.unwrap_or_else(|| PathBuf::from(DEFAULT_CWD));
        path.push(DATA_DIR);
        path.push(network.to_string());
        if !path.exists() {
            fs::create_dir_all(&path)?;
        }
        let conn = Connection::open(path.join(FILE_NAME))?;
        // Create the schema version
        let schema_table_query = format!(
            "CREATE TABLE IF NOT EXISTS {SCHEMA_TABLE_NAME} ({SCHEMA_COLUMN} TEXT PRIMARY KEY, {VERSION_COLUMN} INTEGER NOT NULL)");
        // Update the schema version
        conn.execute(&schema_table_query, [])?;
        let schema_init_version = format!(
            "INSERT OR REPLACE INTO {SCHEMA_TABLE_NAME} ({SCHEMA_COLUMN}, {VERSION_COLUMN}) VALUES (?1, ?2)");
        conn.execute(&schema_init_version, params![SCHEMA_KEY, SCHEMA_VERSION])?;
        // Build the table if it doesn't exist
        conn.execute(INITIAL_HEADER_SCHEMA, [])?;
        // Migrate to any new schema versions
        Self::migrate(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // This function currently does nothing, but if new columns are required this may be used to alter the tables
    // without breaking older tables.
    fn migrate(conn: &Connection) -> Result<(), SqlInitializationError> {
        let version_query =
            format!("SELECT {VERSION_COLUMN} FROM {SCHEMA_TABLE_NAME} WHERE {SCHEMA_COLUMN} = ?1");
        let _current_version: u8 =
            conn.query_row(&version_query, [SCHEMA_KEY], |row| row.get(0))?;
        // Match on the version and migrate to new schemas in the future
        Ok(())
    }

    async fn load<'a>(
        &mut self,
        range: impl RangeBounds<u32> + Send + Sync + 'a,
    ) -> Result<BTreeMap<u32, Header>, SqlHeaderStoreError> {
        let mut param_list = Vec::new();
        let mut stmt = LOAD_QUERY_SELECT_PREFIX.to_string();

        match range.start_bound() {
            Bound::Unbounded => {
                stmt.push_str("WHERE height >= 0 ");
            }
            Bound::Included(h) => {
                stmt.push_str("WHERE height >= ? ");
                param_list.push(*h);
            }
            Bound::Excluded(h) => {
                stmt.push_str("WHERE height > ? ");
                param_list.push(*h);
            }
        };

        match range.end_bound() {
            Bound::Unbounded => (),
            Bound::Included(h) => {
                stmt.push_str("AND height <= ? ");
                param_list.push(*h);
            }
            Bound::Excluded(h) => {
                stmt.push_str("AND height < ? ");
                param_list.push(*h);
            }
        };

        stmt.push_str(LOAD_QUERY_ORDERBY_SUFFIX);

        let mut headers = BTreeMap::<u32, Header>::new();
        let write_lock = self.conn.lock().await;
        let mut query = write_lock.prepare(&stmt)?;
        let mut rows = query.query(params_from_iter(param_list.iter()))?;
        while let Some(row) = rows.next()? {
            let height: u32 = row.get(0)?;
            let hash: String = row.get(1)?;
            let version: i32 = row.get(2)?;
            let prev_hash: String = row.get(3)?;
            let merkle_root: String = row.get(4)?;
            let time: u32 = row.get(5)?;
            let bits: u32 = row.get(6)?;
            let nonce: u32 = row.get(7)?;

            let next_header = Header {
                version: Version::from_consensus(version),
                prev_blockhash: BlockHash::from_str(&prev_hash)
                    .map_err(|_| SqlHeaderStoreError::StringConversion)?,
                merkle_root: TxMerkleNode::from_str(&merkle_root)
                    .map_err(|_| SqlHeaderStoreError::StringConversion)?,
                time,
                bits: CompactTarget::from_consensus(bits),
                nonce,
            };
            if BlockHash::from_str(&hash)
                .map_err(|_| SqlHeaderStoreError::StringConversion)?
                .ne(&next_header.block_hash())
            {
                return Err(SqlHeaderStoreError::Corruption);
            }
            if let Some(header) = headers.values().last() {
                if header.block_hash().ne(&next_header.prev_blockhash) {
                    return Err(SqlHeaderStoreError::Corruption);
                }
            }
            headers.insert(height, next_header);
        }
        Ok(headers)
    }

    async fn write(
        &mut self,
        changes: impl IntoIterator<Item = (u32, &Header)> + Send + Sync,
    ) -> Result<(), SqlHeaderStoreError> {
        let mut write_lock = self.conn.lock().await;
        let tx = write_lock.transaction()?;
        let header_iter = changes.into_iter();
        for (height, header) in header_iter {
            let hash: String = header.block_hash().to_string();
            let version: i32 = header.version.to_consensus();
            let prev_hash: String = header.prev_blockhash.as_raw_hash().to_string();
            let merkle_root: String = header.merkle_root.to_string();
            let time: u32 = header.time;
            let bits: u32 = header.bits.to_consensus();
            let nonce: u32 = header.nonce;
            let stmt = "INSERT OR REPLACE INTO headers (height, block_hash, version, prev_hash, merkle_root, time, bits, nonce) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
            tx.execute(
                stmt,
                params![
                    height,
                    hash,
                    version,
                    prev_hash,
                    merkle_root,
                    time,
                    bits,
                    nonce
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    async fn height_of(
        &mut self,
        block_hash: &BlockHash,
    ) -> Result<Option<u32>, SqlHeaderStoreError> {
        let write_lock = self.conn.lock().await;
        let stmt = "SELECT height FROM headers WHERE block_hash = ?1";
        let row: Option<u32> =
            write_lock.query_row(stmt, params![block_hash.to_string()], |row| row.get(0))?;
        Ok(row)
    }

    async fn hash_at(&mut self, height: u32) -> Result<Option<BlockHash>, SqlHeaderStoreError> {
        let write_lock = self.conn.lock().await;
        let stmt = "SELECT block_hash FROM headers WHERE height = ?1";
        let row: Option<String> = write_lock.query_row(stmt, params![height], |row| row.get(0))?;
        match row {
            Some(row) => match BlockHash::from_str(&row) {
                Ok(hash) => Ok(Some(hash)),
                Err(_) => Err(SqlHeaderStoreError::StringConversion),
            },
            None => Ok(None),
        }
    }

    async fn header_at(&mut self, height: u32) -> Result<Option<Header>, SqlHeaderStoreError> {
        let write_lock = self.conn.lock().await;
        let stmt = "SELECT * FROM headers WHERE height = ?1";
        let query = write_lock.query_row(stmt, params![height], |row| {
            let hash: String = row.get(1)?;
            let version: i32 = row.get(2)?;
            let prev_hash: String = row.get(3)?;
            let merkle_root: String = row.get(4)?;
            let time: u32 = row.get(5)?;
            let bits: u32 = row.get(6)?;
            let nonce: u32 = row.get(7)?;

            let header = Header {
                version: Version::from_consensus(version),
                prev_blockhash: BlockHash::from_str(&prev_hash).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?,
                merkle_root: TxMerkleNode::from_str(&merkle_root).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?,
                time,
                bits: CompactTarget::from_consensus(bits),
                nonce,
            };

            if BlockHash::from_str(&hash)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?
                .ne(&header.block_hash())
            {
                return Err(rusqlite::Error::InvalidQuery);
            }

            Ok(header)
        });
        match query {
            Ok(header) => Ok(Some(header)),
            Err(e) => match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                _ => Err(SqlHeaderStoreError::SQL(e)),
            },
        }
    }
}

impl HeaderStore for SqliteHeaderDb {
    type Error = SqlHeaderStoreError;

    fn load<'a>(
        &'a mut self,
        range: impl RangeBounds<u32> + Send + Sync + 'a,
    ) -> FutureResult<'a, BTreeMap<u32, Header>, Self::Error> {
        Box::pin(self.load(range))
    }

    fn write<'a>(
        &'a mut self,
        changes: impl IntoIterator<Item = (u32, &'a Header)> + Send + Sync + 'a,
    ) -> FutureResult<'a, (), Self::Error> {
        Box::pin(self.write(changes))
    }

    fn height_of<'a>(
        &'a mut self,
        hash: &'a BlockHash,
    ) -> FutureResult<'a, Option<u32>, Self::Error> {
        Box::pin(self.height_of(hash))
    }

    fn hash_at(&mut self, height: u32) -> FutureResult<Option<BlockHash>, Self::Error> {
        Box::pin(self.hash_at(height))
    }

    fn header_at(&mut self, height: u32) -> FutureResult<Option<Header>, Self::Error> {
        Box::pin(self.header_at(height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::consensus::deserialize;

    #[tokio::test]
    async fn test_sql_header_store_normal_use() {
        let binding = tempfile::tempdir().unwrap();
        let path = binding.path();
        let mut db = SqliteHeaderDb::new(Network::Regtest, Some(path.into())).unwrap();
        let block_8: Header = deserialize(&hex::decode("0000002016fe292517eecbbd63227d126a6b1db30ebc5262c61f8f3a4a529206388fc262dfd043cef8454f71f30b5bbb9eb1a4c9aea87390f429721e435cf3f8aa6e2a9171375166ffff7f2000000000").unwrap()).unwrap();
        let block_9: Header = deserialize(&hex::decode("000000205708a90197d93475975545816b2229401ccff7567cb23900f14f2bd46732c605fd8de19615a1d687e89db365503cdf58cb649b8e935a1d3518fa79b0d408704e71375166ffff7f2000000000").unwrap()).unwrap();
        let block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093790c9f554a7780a6043a19619d2a4697364bb62abf6336c0568c31f1eedca3c3e171375166ffff7f2000000000").unwrap()).unwrap();
        let mut map = BTreeMap::new();
        map.insert(8, block_8);
        map.insert(9, block_9);
        map.insert(10, block_10);
        let block_hash_8 = block_8.block_hash();
        let block_hash_9 = block_9.block_hash();
        let w = db
            .write(map.iter().map(|(height, header)| (*height, header)))
            .await;
        assert!(w.is_ok());
        let get_hash_9 = db.hash_at(9).await.unwrap().unwrap();
        assert_eq!(get_hash_9, block_hash_9);
        let get_height_8 = db.height_of(&block_hash_8).await.unwrap().unwrap();
        assert_eq!(get_height_8, 8);
        let load = db.load(7..).await.unwrap();
        assert_eq!(map, load);
        let get_header_9 = db.header_at(9).await.unwrap().unwrap();
        assert_eq!(get_header_9, block_9);
        let get_header_11 = db.header_at(11).await.unwrap();
        assert!(get_header_11.is_none());
        let get_header_7 = db.header_at(7).await.unwrap();
        assert!(get_header_7.is_none());
        drop(db);
        binding.close().unwrap();
    }

    #[tokio::test]
    async fn test_sql_header_loads_with_fork() {
        let binding = tempfile::tempdir().unwrap();
        let path = binding.path();
        let mut db = SqliteHeaderDb::new(Network::Regtest, Some(path.into())).unwrap();
        let block_8: Header = deserialize(&hex::decode("0000002016fe292517eecbbd63227d126a6b1db30ebc5262c61f8f3a4a529206388fc262dfd043cef8454f71f30b5bbb9eb1a4c9aea87390f429721e435cf3f8aa6e2a9171375166ffff7f2000000000").unwrap()).unwrap();
        let block_9: Header = deserialize(&hex::decode("000000205708a90197d93475975545816b2229401ccff7567cb23900f14f2bd46732c605fd8de19615a1d687e89db365503cdf58cb649b8e935a1d3518fa79b0d408704e71375166ffff7f2000000000").unwrap()).unwrap();
        let block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093790c9f554a7780a6043a19619d2a4697364bb62abf6336c0568c31f1eedca3c3e171375166ffff7f2000000000").unwrap()).unwrap();
        let mut map = BTreeMap::new();
        map.insert(8, block_8);
        map.insert(9, block_9);
        map.insert(10, block_10);
        let w = db
            .write(map.iter().map(|(height, header)| (*height, header)))
            .await;
        assert!(w.is_ok());
        let get_height_10 = db.header_at(10).await.unwrap().unwrap();
        assert_eq!(block_10, get_height_10);
        let new_block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093792151c0e9ce4e4c789ca98427d7740cc7acf30d2ca0c08baef266bf152289d814567e5e66ffff7f2001000000").unwrap()).unwrap();
        let block_11: Header = deserialize(&hex::decode("00000020efcf8b12221fccc735b9b0b657ce15b31b9c50aff530ce96a5b4cfe02d8c0068496c1b8a89cf5dec22e46c35ea1035f80f5b666a1b3aa7f3d6f0880d0061adcc567e5e66ffff7f2001000000").unwrap()).unwrap();
        let mut map = BTreeMap::new();
        map.insert(10, new_block_10);
        map.insert(11, block_11);
        let w = db
            .write(map.iter().map(|(height, header)| (*height, header)))
            .await;
        assert!(w.is_ok());
        let block_hash_11 = block_11.block_hash();
        let block_hash_10 = new_block_10.block_hash();
        let w = db
            .write(map.iter().map(|(height, header)| (*height, header)))
            .await;
        assert!(w.is_ok());
        let get_height_10 = db.header_at(10).await.unwrap().unwrap();
        assert_eq!(new_block_10, get_height_10);
        let get_height_12 = db.header_at(12).await.unwrap();
        assert!(get_height_12.is_none());
        let get_hash_10 = db.hash_at(10).await.unwrap().unwrap();
        assert_eq!(get_hash_10, block_hash_10);
        let get_height_11 = db.height_of(&block_hash_11).await.unwrap().unwrap();
        assert_eq!(get_height_11, 11);
        let mut map = BTreeMap::new();
        map.insert(8, block_8);
        map.insert(9, block_9);
        map.insert(10, new_block_10);
        map.insert(11, block_11);
        let load = db.load(7..).await.unwrap();
        assert_eq!(map, load);
        drop(db);
        binding.close().unwrap();
    }

    #[tokio::test]
    async fn test_range_loads_properly() {
        let binding = tempfile::tempdir().unwrap();
        let path = binding.path();
        let mut db = SqliteHeaderDb::new(Network::Regtest, Some(path.into())).unwrap();
        let block_8: Header = deserialize(&hex::decode("0000002016fe292517eecbbd63227d126a6b1db30ebc5262c61f8f3a4a529206388fc262dfd043cef8454f71f30b5bbb9eb1a4c9aea87390f429721e435cf3f8aa6e2a9171375166ffff7f2000000000").unwrap()).unwrap();
        let block_9: Header = deserialize(&hex::decode("000000205708a90197d93475975545816b2229401ccff7567cb23900f14f2bd46732c605fd8de19615a1d687e89db365503cdf58cb649b8e935a1d3518fa79b0d408704e71375166ffff7f2000000000").unwrap()).unwrap();
        let block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093790c9f554a7780a6043a19619d2a4697364bb62abf6336c0568c31f1eedca3c3e171375166ffff7f2000000000").unwrap()).unwrap();
        let mut map = BTreeMap::new();
        map.insert(8, block_8);
        map.insert(9, block_9);
        map.insert(10, block_10);
        let w = db
            .write(map.iter().map(|(height, header)| (*height, header)))
            .await;
        assert!(w.is_ok());
        let load = db.load(7..).await.unwrap();
        assert_eq!(map, load);
        let load = db.load(8..).await.unwrap();
        assert_eq!(map, load);
        let load = db.load(8..10).await.unwrap();
        map.remove(&10);
        assert_eq!(map, load);
        let load = db.load(..10).await.unwrap();
        assert_eq!(map, load);
        drop(db);
        binding.close().unwrap();
    }
}
