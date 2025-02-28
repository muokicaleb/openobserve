// Copyright 2023 Zinc Labs Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{
    cmp::min,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use ahash::HashMap;
use async_trait::async_trait;
use bytes::Bytes;
use config::CONFIG;
use etcd_client::{
    Certificate, DeleteOptions, EventType, GetOptions, Identity, SortOrder, SortTarget, TlsOptions,
};
use tokio::{
    sync::{mpsc, OnceCell},
    task::JoinHandle,
    time,
};

use crate::common::infra::{
    cluster,
    db::{Event, EventData},
    errors::*,
};

static ETCD_CLIENT: OnceCell<etcd_client::Client> = OnceCell::const_new();

pub async fn get_etcd_client() -> &'static etcd_client::Client {
    ETCD_CLIENT.get_or_init(connect_etcd).await
}

pub async fn init() {
    // enable keep alive for auth token
    tokio::task::spawn(async move { keepalive_connection().await });
}

pub struct Etcd {
    prefix: String,
}

impl Etcd {
    pub fn new(prefix: &str) -> Etcd {
        let prefix = prefix.trim_end_matches(|v| v == '/');
        Etcd {
            prefix: prefix.to_string(),
        }
    }
}

impl Default for Etcd {
    fn default() -> Self {
        Self::new(&CONFIG.etcd.prefix)
    }
}

#[async_trait]
impl super::Db for Etcd {
    async fn create_table(&self) -> Result<()> {
        Ok(())
    }

    async fn stats(&self) -> Result<super::Stats> {
        let mut client = get_etcd_client().await.clone();
        let stats = client.status().await?;
        let bytes_len = stats.db_size();
        let resp = client
            .get(
                "",
                Some(GetOptions::new().with_all_keys().with_count_only()),
            )
            .await?;
        let keys_count = resp.count();
        Ok(super::Stats {
            bytes_len,
            keys_count,
        })
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let key = format!("{}{}", self.prefix, key);
        let mut client = get_etcd_client().await.clone();
        let ret = client.get(key.as_str(), None).await?;
        if ret.kvs().is_empty() {
            return Err(Error::from(DbError::KeyNotExists(key)));
        }
        Ok(Bytes::from(ret.kvs()[0].value().to_vec()))
    }

    async fn put(&self, key: &str, value: Bytes, _need_watch: bool) -> Result<()> {
        let key = format!("{}{}", self.prefix, key);
        let mut client = get_etcd_client().await.clone();
        let _ = client.put(key, value, None).await?;
        Ok(())
    }

    async fn delete(&self, key: &str, with_prefix: bool, _need_watch: bool) -> Result<()> {
        let key = format!("{}{}", self.prefix, key);
        let mut client = get_etcd_client().await.clone();
        let opt = with_prefix.then(|| DeleteOptions::new().with_prefix());
        let _ = client.delete(key.as_str(), opt).await?.deleted();
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<HashMap<String, Bytes>> {
        let mut result = HashMap::default();
        let key = format!("{}{}", self.prefix, prefix);
        let mut client = get_etcd_client().await.clone();
        let mut opt = GetOptions::new()
            .with_prefix()
            .with_sort(SortTarget::Key, SortOrder::Ascend)
            .with_limit(CONFIG.etcd.load_page_size);
        let mut resp = client.get(key.clone(), Some(opt.clone())).await?;
        let mut first_call = true;
        let mut have_next = true;
        let mut last_key = String::new();
        loop {
            let kvs_num = resp.kvs().len() as i64;
            if kvs_num < CONFIG.etcd.load_page_size {
                have_next = false;
            }
            for kv in resp.kvs() {
                let item_key = kv.key_str().unwrap();
                if !item_key.starts_with(&key) {
                    have_next = false;
                    break;
                }
                if item_key.eq(last_key.as_str()) {
                    continue;
                }
                let item_key = item_key.strip_prefix(&self.prefix).unwrap();
                result.insert(item_key.to_string(), Bytes::from(kv.value().to_vec()));
            }
            tokio::task::yield_now().await; // yield to other tasks

            if !have_next {
                break;
            }
            if first_call {
                first_call = false;
                opt = opt.with_from_key();
            }
            last_key = resp.kvs().last().unwrap().key_str().unwrap().to_string();
            resp = client.get(last_key.clone(), Some(opt.clone())).await?;
        }
        Ok(result)
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let key = format!("{}{}", self.prefix, prefix);
        let mut client = get_etcd_client().await.clone();
        let mut opt = GetOptions::new()
            .with_prefix()
            .with_sort(SortTarget::Key, SortOrder::Ascend)
            .with_limit(CONFIG.etcd.load_page_size);
        let mut resp = client.get(key.clone(), Some(opt.clone())).await?;
        let mut first_call = true;
        let mut have_next = true;
        let mut last_key = String::new();
        loop {
            let kvs_num = resp.kvs().len() as i64;
            if kvs_num < CONFIG.etcd.load_page_size {
                have_next = false;
            }
            for kv in resp.kvs() {
                let item_key = kv.key_str().unwrap();
                if !item_key.starts_with(&key) {
                    have_next = false;
                    break;
                }
                if item_key.eq(last_key.as_str()) {
                    continue;
                }
                let item_key = item_key.strip_prefix(&self.prefix).unwrap();
                result.push(item_key.to_string());
            }
            tokio::task::yield_now().await; // yield to other tasks

            if !have_next {
                break;
            }
            if first_call {
                first_call = false;
                opt = opt.with_from_key();
            }
            last_key = resp.kvs().last().unwrap().key_str().unwrap().to_string();
            resp = client.get(last_key.clone(), Some(opt.clone())).await?;
        }
        Ok(result)
    }

    async fn list_values(&self, prefix: &str) -> Result<Vec<Bytes>> {
        let mut result = Vec::new();
        let key = format!("{}{}", self.prefix, prefix);
        let mut client = get_etcd_client().await.clone();
        let mut opt = GetOptions::new()
            .with_prefix()
            .with_sort(SortTarget::Key, SortOrder::Ascend)
            .with_limit(CONFIG.etcd.load_page_size);
        let mut resp = client.get(key.clone(), Some(opt.clone())).await?;
        let mut first_call = true;
        let mut have_next = true;
        let mut last_key = String::new();
        loop {
            let kvs_num = resp.kvs().len() as i64;
            if kvs_num < CONFIG.etcd.load_page_size {
                have_next = false;
            }
            for kv in resp.kvs() {
                let item_key = kv.key_str().unwrap();
                if !item_key.starts_with(&key) {
                    have_next = false;
                    break;
                }
                if item_key.eq(last_key.as_str()) {
                    continue;
                }
                result.push(Bytes::from(kv.value().to_vec()));
            }
            tokio::task::yield_now().await; // yield to other tasks

            if !have_next {
                break;
            }
            if first_call {
                first_call = false;
                opt = opt.with_from_key();
            }
            last_key = resp.kvs().last().unwrap().key_str().unwrap().to_string();
            resp = client.get(last_key.clone(), Some(opt.clone())).await?;
        }
        Ok(result)
    }

    async fn count(&self, prefix: &str) -> Result<i64> {
        let key = format!("{}{}", self.prefix, prefix);
        let mut client = get_etcd_client().await.clone();
        let opt = GetOptions::new().with_prefix().with_count_only();
        let resp = client.get(key.clone(), Some(opt)).await?;
        Ok(resp.count())
    }

    async fn watch(&self, prefix: &str) -> Result<Arc<mpsc::Receiver<Event>>> {
        let (tx, rx) = mpsc::channel(1024);
        let key = format!("{}{}", &self.prefix, prefix);
        let prefix_key = self.prefix.to_string();
        let _task: JoinHandle<Result<()>> = tokio::task::spawn(async move {
            loop {
                if cluster::is_offline() {
                    break;
                }
                let mut client = get_etcd_client().await.clone();
                let opt = etcd_client::WatchOptions::new().with_prefix();
                let (mut _watcher, mut stream) =
                    match client.watch(key.clone(), Some(opt.clone())).await {
                        Ok((watcher, stream)) => (watcher, stream),
                        Err(e) => {
                            log::error!("watching prefix: {}, error: {}", key, e);
                            time::sleep(time::Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                loop {
                    let resp = match stream.message().await {
                        Ok(resp) => resp,
                        Err(e) => {
                            log::error!("watching prefix: {}, get message error: {}", key, e);
                            break;
                        }
                    };
                    if let Some(ev) = resp {
                        for ev in ev.events() {
                            let kv = ev.kv().unwrap();
                            let item_key = kv.key_str().unwrap();
                            let item_key = item_key.strip_prefix(&prefix_key).unwrap();
                            match ev.event_type() {
                                EventType::Put => tx
                                    .send(Event::Put(EventData {
                                        key: item_key.to_string(),
                                        value: Some(Bytes::from(kv.value().to_vec())),
                                    }))
                                    .await
                                    .unwrap(),
                                EventType::Delete => tx
                                    .send(Event::Delete(EventData {
                                        key: item_key.to_string(),
                                        value: None,
                                    }))
                                    .await
                                    .unwrap(),
                            }
                        }
                    }
                }
            }
            Ok(())
        });
        Ok(Arc::new(rx))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

pub async fn create_table() -> Result<()> {
    Ok(())
}

pub async fn connect_etcd() -> etcd_client::Client {
    if CONFIG.common.print_key_config {
        log::info!("etcd init config: {:?}", CONFIG.etcd);
    }

    let mut opts = etcd_client::ConnectOptions::new()
        .with_timeout(core::time::Duration::from_secs(CONFIG.etcd.command_timeout))
        .with_connect_timeout(core::time::Duration::from_secs(CONFIG.etcd.connect_timeout));
    if !&CONFIG.etcd.user.is_empty() {
        opts = opts.with_user(&CONFIG.etcd.user, &CONFIG.etcd.password);
    }
    if CONFIG.etcd.cert_auth {
        let server_root_ca_cert = tokio::fs::read(&CONFIG.etcd.ca_file).await.unwrap();
        let server_root_ca_cert = Certificate::from_pem(server_root_ca_cert);
        let client_cert = tokio::fs::read(&CONFIG.etcd.cert_file).await.unwrap();
        let client_key = tokio::fs::read(&CONFIG.etcd.key_file).await.unwrap();
        let client_identity = Identity::from_pem(client_cert, client_key);
        let tls = TlsOptions::new()
            .domain_name(&CONFIG.etcd.domain_name)
            .ca_certificate(server_root_ca_cert)
            .identity(client_identity);
        opts = opts.with_tls(tls);
    }
    let addrs = CONFIG.etcd.addr.split(',').collect::<Vec<&str>>();
    etcd_client::Client::connect(addrs, Some(opts))
        .await
        .expect("Etcd connect failed")
}

pub async fn keepalive_connection() -> Result<()> {
    loop {
        if cluster::is_offline() {
            break;
        }
        let mut client = get_etcd_client().await.clone();
        let key = format!("{}healthz", &CONFIG.etcd.prefix);
        let key = key.as_str();
        client.put(key, "OK", None).await?;
        let mut interval = time::interval(time::Duration::from_secs(60));
        interval.tick().await; // trigger the first run
        loop {
            interval.tick().await;
            match client.get(key, None).await {
                Ok(ret) => for _item in ret.kvs() {},
                Err(e) => {
                    log::error!("keep alive connection error: {:?}", e);
                    break;
                }
            };
        }
    }

    Ok(())
}

pub async fn keepalive_lease_id<F>(id: i64, ttl: i64, stopper: F) -> Result<()>
where
    F: Fn() -> bool,
{
    let mut ttl_keep_alive = min(10, (ttl / 2) as u64);
    loop {
        if stopper() {
            break;
        }
        let mut client = get_etcd_client().await.clone();
        let (mut keeper, mut stream) = match client.lease_keep_alive(id).await {
            Ok((keeper, stream)) => (keeper, stream),
            Err(e) => {
                log::error!("lease {:?} keep alive error: {:?}", id, e);
                time::sleep(time::Duration::from_secs(1)).await;
                continue;
            }
        };
        loop {
            if stopper() {
                break;
            }
            time::sleep(time::Duration::from_secs(ttl_keep_alive)).await;
            match keeper.keep_alive().await {
                Ok(_) => {}
                Err(e) => {
                    log::error!("lease {:?} keep alive do keeper error: {:?}", id, e);
                    ttl_keep_alive = 1;
                    break;
                }
            }
            match stream.message().await {
                Ok(v) => {
                    if v.unwrap().ttl() == 0 {
                        log::error!("lease {:?} keep alive ttl is 0", id);
                        return Err(Error::from(etcd_client::Error::LeaseKeepAliveError(
                            "lease expired or revoked".to_string(),
                        )));
                    }
                    ttl_keep_alive = min(10, (ttl / 2) as u64);
                }
                Err(e) => {
                    log::error!("lease {:?} keep alive receive message: {:?}", id, e);
                    ttl_keep_alive = 1;
                    break;
                }
            };
        }
    }

    Ok(())
}

pub struct Locker {
    key: String,
    lock_id: String,
    state: Arc<AtomicU8>, // 0: init, 1: locking, 2: release
}

impl Locker {
    pub fn new(key: &str) -> Self {
        let key = format!("{}lock/{}", &CONFIG.etcd.prefix, key);
        Self {
            key,
            lock_id: "".to_string(),
            state: Arc::new(AtomicU8::new(0)),
        }
    }

    /// lock with timeout, 0 means use default timeout, unit: second
    pub async fn lock(&mut self, timeout: u64) -> Result<()> {
        let mut client = get_etcd_client().await.clone();
        let mut last_err = None;
        let timeout = if timeout == 0 {
            CONFIG.etcd.lock_wait_timeout
        } else {
            timeout
        };
        let mut n = timeout / CONFIG.etcd.command_timeout;
        if n < 1 {
            n = 1;
        }
        for _ in 0..n {
            match client.lock(self.key.as_str(), None).await {
                Ok(resp) => {
                    self.lock_id = String::from_utf8_lossy(resp.key()).to_string();
                    self.state.store(1, Ordering::SeqCst);
                    last_err = None;
                    break;
                }
                Err(err) => {
                    last_err = Some(err.to_string());
                    if !err.to_string().contains("Timeout expired") {
                        break;
                    }
                }
            };
        }
        if let Some(err) = last_err {
            return Err(Error::Message(format!("etcd lock error: {err}")));
        }
        Ok(())
    }

    pub async fn unlock(&self) -> Result<()> {
        if self.state.load(Ordering::SeqCst) != 1 {
            return Ok(());
        }
        let mut client = get_etcd_client().await.clone();
        match client.unlock(self.lock_id.as_str()).await {
            Ok(_) => {}
            Err(err) => {
                log::error!("etcd unlock error: {}, key: {}", err, self.key);
                return Err(Error::Message("etcd unlock error".to_string()));
            }
        };
        self.state.store(2, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{super::Db, *};

    #[tokio::test]
    async fn test_etcd_prefix() {
        let client = Etcd::default();
        assert_eq!(client.prefix, "/zinc/observe".to_string());
    }

    #[tokio::test]
    async fn test_etcd_count() {
        if CONFIG.common.local_mode {
            return;
        }
        let client = Etcd::default();
        client
            .put("/test/count/1", bytes::Bytes::from("1"), false)
            .await
            .unwrap();
        client
            .put("/test/count/2", bytes::Bytes::from("2"), false)
            .await
            .unwrap();
        client
            .put("/test/count/3", bytes::Bytes::from("3"), false)
            .await
            .unwrap();
        let count = client.count("/test/count").await.unwrap();
        assert_eq!(count, 3);
    }
}
