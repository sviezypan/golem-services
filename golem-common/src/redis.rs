// Copyright 2024 Golem Cloud
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::atomic::AtomicBool;
use std::sync::{atomic, Arc};
use std::time::Instant;

use bincode::{Decode, Encode};
use bytes::Bytes;
use fred::clients::Transaction;
use fred::prelude::{RedisPool as FredRedisPool, *};
use fred::types::{
    Limit, MultipleKeys, MultipleOrderedPairs, MultipleValues, MultipleZaddValues, Ordering,
    RedisKey, RedisMap, XCap, ZRange, ZSort, XID,
};
use serde::de::DeserializeOwned;
use tracing::Level;

use crate::metrics::redis::{record_redis_failure, record_redis_success};
use crate::serialization::{deserialize, serialize};

#[derive(Clone, Debug)]
pub struct RedisPool {
    pool: FredRedisPool,
    key_prefix: String,
    connected: Arc<AtomicBool>,
}

impl RedisPool {
    pub fn new(pool: FredRedisPool, key_prefix: String) -> Self {
        Self {
            pool,
            key_prefix,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn configured(config: &crate::config::RedisConfig) -> Result<RedisPool, RedisError> {
        let mut redis_config = RedisConfig::from_url(config.url().as_str())?;
        redis_config.tracing = TracingConfig::new(config.tracing);
        redis_config.tracing.default_tracing_level = Level::DEBUG;
        redis_config.username = config.username.clone();
        redis_config.password = config.password.clone();

        let policy = ReconnectPolicy::new_exponential(
            config.retries.max_attempts,
            config.retries.min_delay.as_millis() as u32,
            config.retries.max_delay.as_millis() as u32,
            config.retries.multiplier,
        );
        let pool = FredRedisPool::new(redis_config, None, None, Some(policy), config.pool_size)?;

        Ok(RedisPool {
            pool,
            key_prefix: config.key_prefix.clone(),
            connected: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn with<'a>(
        &'a self,
        svc_name: &'static str,
        api_name: &'static str,
    ) -> RedisLabelledApi<'a> {
        RedisLabelledApi {
            svc_name,
            api_name,
            pool: self.pool.clone(),
            key_prefix: self.key_prefix.clone(),
            connected: &self.connected,
        }
    }

    pub fn serialize<T: Encode>(&self, value: &T) -> Result<Bytes, String> {
        serialize(value)
    }

    pub fn deserialize<T: DeserializeOwned + Decode>(&self, bytes: &[u8]) -> Result<T, String> {
        deserialize(bytes)
    }
}

pub struct RedisLabelledApi<'a> {
    svc_name: &'static str,
    api_name: &'static str,
    pool: FredRedisPool,
    key_prefix: String,
    connected: &'a AtomicBool,
}

impl<'a> RedisLabelledApi<'a> {
    pub async fn ensure_connected(&self) -> Result<(), RedisError> {
        if !self.connected.swap(true, atomic::Ordering::Relaxed) {
            let _connection = self.pool.connect();
            self.pool.wait_for_connect().await?;
        }
        Ok(())
    }

    fn record<R>(
        &self,
        start: Instant,
        cmd_name: &'static str,
        result: RedisResult<R>,
    ) -> RedisResult<R> {
        let end = Instant::now();
        match result {
            Ok(result) => {
                record_redis_success(
                    self.svc_name,
                    self.api_name,
                    cmd_name,
                    end.duration_since(start),
                );
                Ok(result)
            }
            Err(err) => {
                record_redis_failure(self.svc_name, self.api_name, cmd_name);
                Err(err)
            }
        }
    }

    fn prefixed_key<K>(&self, key: K) -> String
    where
        K: AsRef<str>,
    {
        format!("{}{}", &self.key_prefix, key.as_ref())
    }

    pub async fn del<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(start, "DEL", self.pool.del(self.prefixed_key(key)).await)
    }

    pub async fn get<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(start, "GET", self.pool.get(self.prefixed_key(key)).await)
    }

    pub async fn exists<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "EXISTS",
            self.pool.exists(self.prefixed_key(key)).await,
        )
    }

    pub async fn mget<R, K>(&self, keys: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: Into<MultipleKeys> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(start, "MGET", self.pool.mget(keys).await)
    }

    pub async fn hdel<R, K, F>(&self, key: K, fields: F) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        F: Into<MultipleKeys> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HDEL",
            self.pool.hdel(self.prefixed_key(key), fields).await,
        )
    }

    pub async fn hexists<R, K, F>(&self, key: K, field: F) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        F: Into<RedisKey> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HEXISTS",
            self.pool.hexists(self.prefixed_key(key), field).await,
        )
    }

    pub async fn hget<R, K, F>(&self, key: K, field: F) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        F: Into<RedisKey> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HGET",
            self.pool.hget(self.prefixed_key(key), field).await,
        )
    }

    pub async fn hkeys<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HKEYS",
            self.pool.hkeys(self.prefixed_key(key)).await,
        )
    }

    pub async fn hmget<R, K, F>(&self, key: K, fields: F) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        F: Into<MultipleKeys> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HMGET",
            self.pool.hmget(self.prefixed_key(key), fields).await,
        )
    }

    pub async fn hmset<R, K, V>(&self, key: K, values: V) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<RedisMap> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HMSET",
            self.pool.hmset(self.prefixed_key(key), values).await,
        )
    }

    pub async fn hset<R, K, V>(&self, key: K, values: V) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<RedisMap> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "HSET",
            self.pool.hset(self.prefixed_key(key), values).await,
        )
    }

    pub async fn sadd<R, K, V>(&self, key: K, members: V) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<MultipleValues> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "SADD",
            self.pool.sadd(self.prefixed_key(key), members).await,
        )
    }

    pub async fn set<R, K, V>(
        &self,
        key: K,
        value: V,
        expire: Option<Expiration>,
        options: Option<SetOptions>,
        get: bool,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<RedisValue> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "SET",
            self.pool
                .set(self.prefixed_key(key), value, expire, options, get)
                .await,
        )
    }

    pub async fn smembers<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "SMEMBERS",
            self.pool.smembers(self.prefixed_key(key)).await,
        )
    }

    pub async fn srem<R, K, V>(&self, key: K, members: V) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<MultipleValues> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "SREM",
            self.pool.srem(self.prefixed_key(key), members).await,
        )
    }

    pub async fn scard<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "SCARD",
            self.pool.scard(self.prefixed_key(key)).await,
        )
    }

    pub async fn xadd<R, K, C, I, F>(
        &self,
        key: K,
        nomkstream: bool,
        cap: C,
        id: I,
        fields: F,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        I: Into<XID> + Send,
        F: TryInto<MultipleOrderedPairs> + Send,
        F::Error: Into<RedisError> + Send,
        C: TryInto<XCap> + Send,
        C::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "XADD",
            self.pool
                .xadd(self.prefixed_key(key), nomkstream, cap, id, fields)
                .await,
        )
    }

    pub async fn xlen<R, K>(&self, key: K) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(start, "XLEN", self.pool.xlen(self.prefixed_key(key)).await)
    }

    pub async fn xrange<R, K, S, E>(
        &self,
        key: K,
        start: S,
        end: E,
        count: Option<u64>,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        S: TryInto<RedisValue> + Send,
        S::Error: Into<RedisError> + Send,
        E: TryInto<RedisValue> + Send,
        E::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start_time = Instant::now();
        self.record(
            start_time,
            "XRANGE",
            self.pool
                .xrange(self.prefixed_key(key), start, end, count)
                .await,
        )
    }

    pub async fn xrevrange<R, K, S, E>(
        &self,
        key: K,
        end: E,
        start: S,
        count: Option<u64>,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        S: TryInto<RedisValue> + Send,
        S::Error: Into<RedisError> + Send,
        E: TryInto<RedisValue> + Send,
        E::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start_time = Instant::now();
        self.record(
            start_time,
            "XREVRANGE",
            self.pool
                .xrevrange(self.prefixed_key(key), end, start, count)
                .await,
        )
    }

    pub async fn xtrim<R, K, C>(&self, key: K, cap: C) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        C: TryInto<XCap> + Send,
        C::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "XTRIM",
            self.pool.xtrim(self.prefixed_key(key), cap).await,
        )
    }

    pub async fn zadd<R, K, V>(
        &self,
        key: K,
        options: Option<SetOptions>,
        ordering: Option<Ordering>,
        changed: bool,
        incr: bool,
        values: V,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<MultipleZaddValues> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "ZADD",
            self.pool
                .zadd(
                    self.prefixed_key(key),
                    options,
                    ordering,
                    changed,
                    incr,
                    values,
                )
                .await,
        )
    }

    pub async fn zrange<R, K, M, N>(
        &self,
        key: K,
        min: M,
        max: N,
        sort: Option<ZSort>,
        rev: bool,
        limit: Option<Limit>,
        withscores: bool,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        M: TryInto<ZRange> + Send,
        M::Error: Into<RedisError> + Send,
        N: TryInto<ZRange> + Send,
        N::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "ZRANGE",
            self.pool
                .zrange(
                    self.prefixed_key(key),
                    min,
                    max,
                    sort,
                    rev,
                    limit,
                    withscores,
                )
                .await,
        )
    }

    pub async fn zrangebyscore<R, K, M, N>(
        &self,
        key: K,
        min: M,
        max: N,
        withscores: bool,
        limit: Option<Limit>,
    ) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        M: TryInto<ZRange> + Send,
        M::Error: Into<RedisError> + Send,
        N: TryInto<ZRange> + Send,
        N::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "ZRANGEBYSCORE",
            self.pool
                .zrangebyscore(self.prefixed_key(key), min, max, withscores, limit)
                .await,
        )
    }

    pub async fn zrem<R, K, V>(&self, key: K, members: V) -> RedisResult<R>
    where
        R: FromRedis,
        K: AsRef<str>,
        V: TryInto<MultipleValues> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.ensure_connected().await?;
        let start = Instant::now();
        self.record(
            start,
            "ZREM",
            self.pool.zrem(self.prefixed_key(key), members).await,
        )
    }

    pub async fn transaction<R, F, Fu>(&self, func: F) -> RedisResult<R>
    where
        R: FromRedis,
        F: FnOnce(RedisTransaction) -> Fu,
        Fu: std::future::Future<Output = RedisResult<RedisTransaction>>,
    {
        self.ensure_connected().await?;
        let start = Instant::now();

        let client = self.pool.next_connected();
        let trx = client.multi();
        let trx = RedisTransaction::new(trx, self.key_prefix.clone());
        let trx = func(trx).await?;

        self.record(start, "MULTI", trx.trx.exec(true).await)
    }
}

pub struct RedisTransaction {
    trx: Transaction,
    key_prefix: String,
}

impl RedisTransaction {
    fn new(trx: Transaction, key_prefix: String) -> Self {
        Self { trx, key_prefix }
    }

    fn prefixed_key<K>(&self, key: K) -> String
    where
        K: AsRef<str>,
    {
        format!("{}{}", &self.key_prefix, key.as_ref())
    }

    pub async fn del<K>(&self, key: K) -> RedisResult<()>
    where
        K: AsRef<str>,
    {
        self.trx.del(self.prefixed_key(key)).await
    }

    pub async fn set<K, V>(
        &self,
        key: K,
        value: V,
        expire: Option<Expiration>,
        options: Option<SetOptions>,
        get: bool,
    ) -> RedisResult<()>
    where
        K: AsRef<str>,
        V: TryInto<RedisValue> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.trx
            .set(self.prefixed_key(key), value, expire, options, get)
            .await
    }

    pub async fn sadd<K, V>(&self, key: K, members: V) -> RedisResult<()>
    where
        K: AsRef<str>,
        V: TryInto<MultipleValues> + Send,
        V::Error: Into<RedisError> + Send,
    {
        self.trx.sadd(self.prefixed_key(key), members).await
    }

    pub async fn srem<K>(&self, key: K, members: Vec<String>) -> RedisResult<()>
    where
        K: AsRef<str>,
    {
        self.trx.srem(self.prefixed_key(key), members).await
    }

    pub async fn scard<K>(&self, key: K) -> RedisResult<()>
    where
        K: AsRef<str>,
    {
        self.trx.scard(self.prefixed_key(key)).await
    }
}
