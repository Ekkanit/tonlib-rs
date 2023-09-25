use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Instant;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::{broadcast, oneshot};

use crate::client::{
    TonConnectionCallback, TonConnectionParams, TonFunctions, TonNotificationReceiver,
};
use crate::tl::TonFunction;
use crate::tl::TonNotification;
use crate::tl::TonResult;
use crate::tl::TvmStackEntry;
use crate::tl::{Config, KeyStoreType, Options, OptionsInfo, SmcMethodId, SmcRunResult};
use crate::tl::{TlTonClient, TonResultDiscriminants};

use super::error::TonClientError;

struct RequestData {
    method: &'static str,
    send_time: Instant,
    sender: oneshot::Sender<Result<TonResult, TonClientError>>,
}

type RequestMap = DashMap<u32, RequestData>;
type TonNotificationSender = broadcast::Sender<Arc<TonNotification>>;

struct Inner {
    tl_client: TlTonClient,
    counter: AtomicU32,
    request_map: RequestMap,
    notification_sender: TonNotificationSender,
    callback: Arc<dyn TonConnectionCallback + Send + Sync>,
    _notification_receiver: TonNotificationReceiver,
}

pub struct TonConnection {
    inner: Arc<Inner>,
}

static CONNECTION_COUNTER: AtomicU32 = AtomicU32::new(0);

impl TonConnection {
    /// Creates a new uninitialized TonConnection
    ///
    /// # Errors
    ///
    /// Returns error to capture any failure to create thread at system level
    pub fn new(
        callback: Arc<dyn TonConnectionCallback + Send + Sync>,
    ) -> Result<TonConnection, TonClientError> {
        let tag = format!(
            "ton-conn-{}",
            CONNECTION_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let (sender, receiver) = broadcast::channel::<Arc<TonNotification>>(10000); // TODO: Configurable
        let inner = Inner {
            tl_client: TlTonClient::new(tag.as_str()),
            counter: AtomicU32::new(0),
            request_map: RequestMap::new(),
            notification_sender: sender,
            callback,
            _notification_receiver: receiver,
        };
        let client = TonConnection {
            inner: Arc::new(inner),
        };
        let client_inner: Weak<Inner> = Arc::downgrade(&client.inner);
        let thread_builder = thread::Builder::new().name(tag.clone());
        thread_builder.spawn(|| run_loop(tag, client_inner))?;
        Ok(client)
    }

    /// Creates a new initialized TonConnection
    pub async fn connect(
        params: &TonConnectionParams,
        callback: Arc<dyn TonConnectionCallback + Send + Sync>,
    ) -> Result<TonConnection, TonClientError> {
        let conn = Self::new(callback)?;
        let keystore_type = if let Some(directory) = &params.keystore_dir {
            KeyStoreType::Directory {
                directory: directory.clone(),
            }
        } else {
            KeyStoreType::InMemory
        };
        let _ = conn
            .init(
                params.config.as_str(),
                params.blockchain_name.as_deref(),
                params.use_callbacks_for_network,
                params.ignore_cache,
                keystore_type,
            )
            .await?;
        Ok(conn)
    }

    /// Attempts to initialize an existing TonConnection
    pub async fn init(
        &self,
        config: &str,
        blockchain_name: Option<&str>,
        use_callbacks_for_network: bool,
        ignore_cache: bool,
        keystore_type: KeyStoreType,
    ) -> Result<OptionsInfo, TonClientError> {
        let func = TonFunction::Init {
            options: Options {
                config: Config {
                    config: String::from(config),
                    blockchain_name: blockchain_name.map(|s| String::from(s)),
                    use_callbacks_for_network,
                    ignore_cache,
                },
                keystore_type,
            },
        };
        let result = self.invoke(&func).await?;
        match result {
            TonResult::OptionsInfo(options_info) => Ok(options_info),
            r => Err(TonClientError::unexpected_ton_result(
                TonResultDiscriminants::OptionsInfo.into(),
                r,
            )),
        }
    }

    pub fn subscribe(&self) -> TonNotificationReceiver {
        self.inner.notification_sender.subscribe()
    }

    pub async fn smc_run_get_method(
        &self,
        id: i64,
        method: &SmcMethodId,
        stack: &Vec<TvmStackEntry>,
    ) -> Result<SmcRunResult, TonClientError> {
        let func = TonFunction::SmcRunGetMethod {
            id: id,
            method: method.clone(),
            stack: stack.to_vec(),
        };
        let result = self.invoke(&func).await?;
        match result {
            TonResult::SmcRunResult(result) => Ok(result),
            r => Err(TonClientError::unexpected_ton_result(
                TonResultDiscriminants::SmcRunResult,
                r,
            )),
        }
    }
}

#[async_trait]
impl TonFunctions for TonConnection {
    async fn get_connection(&self) -> Result<TonConnection, TonClientError> {
        Ok(self.clone())
    }

    async fn invoke_on_connection(
        &self,
        function: &TonFunction,
    ) -> Result<(TonConnection, TonResult), TonClientError> {
        let cnt = self.inner.counter.fetch_add(1, Ordering::SeqCst);
        let extra = cnt.to_string();
        let (tx, rx) = oneshot::channel::<Result<TonResult, TonClientError>>();
        let data = RequestData {
            method: function.into(),
            send_time: Instant::now(),
            sender: tx,
        };
        self.inner.request_map.insert(cnt, data);
        self.inner.callback.on_invoke(cnt);
        let res = self.inner.tl_client.send(function, extra.as_str());
        if let Err(e) = res {
            let (_, data) = self.inner.request_map.remove(&cnt).unwrap();
            self.inner.callback.on_tl_error(&e);
            let err = TonClientError::TlError(e);
            data.sender.send(Err(err)).unwrap(); // Send should always succeed, so something went terribly wrong
        }
        let maybe_result = rx.await;
        let result = match maybe_result {
            Ok(result) => result,
            Err(_) => return Err(TonClientError::InternalError),
        };
        result.map(|r| (self.clone(), r))
    }
}

impl Clone for TonConnection {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        TonConnection { inner }
    }
}

/// Client run loop
fn run_loop(tag: String, weak_inner: Weak<Inner>) {
    log::info!("[{}] Starting event loop", tag);
    loop {
        if let Some(inner) = weak_inner.upgrade() {
            let recv = inner.tl_client.receive(1.0);
            if let Some((ton_result, maybe_extra)) = recv {
                let maybe_request_id = maybe_extra.and_then(|s| s.parse::<u32>().ok());
                let maybe_data = maybe_request_id.and_then(|i| inner.request_map.remove(&i));
                let result: Result<TonResult, TonClientError> = match ton_result {
                    Ok(TonResult::Error { code, message }) => {
                        inner
                            .callback
                            .on_tonlib_error(&maybe_request_id, code, &message);
                        Err(TonClientError::TonlibError { code, message })
                    }
                    Err(e) => {
                        log::warn!("[{}] Tonlib error: {}", tag, e,);
                        inner.callback.on_tl_error(&e);
                        Err(e.into())
                    }
                    Ok(r) => Ok(r),
                };

                match maybe_data {
                    Some((_, data)) => {
                        let request_id = maybe_request_id.unwrap(); // Can't be empty if data is not empty
                        let now = Instant::now();
                        let duration = now.duration_since(data.send_time);
                        inner.callback.on_invoke_result(
                            request_id,
                            data.method,
                            &duration,
                            &result,
                        );
                        log::debug!(
                            "[{}] Invoke successful, request_id: {}, method: {}, elapsed: {:?}",
                            tag,
                            request_id,
                            data.method,
                            &duration
                        );
                        if let Err(e) = data.sender.send(result) {
                            inner
                                .callback
                                .on_invoke_result_send_error(request_id, &duration, &e);
                            log::warn!(
                                "[{}] Error sending invoke result, method: {} request_id: {}: {:?}",
                                tag,
                                data.method,
                                request_id,
                                e
                            );
                        }
                    }
                    None => {
                        if let Ok(r) = result {
                            // Errors are ignored
                            let maybe_notification = TonNotification::from_result(&r);
                            if let Some(n) = maybe_notification {
                                inner.callback.on_notification(&n);
                                if let Err(e) = inner.notification_sender.send(Arc::new(n)) {
                                    log::warn!("[{}] Error sending notification: {}", tag, e);
                                }
                            } else {
                                inner.callback.on_ton_result_parse_error(&r);
                                log::warn!("[{}] Error parsing result: {}", tag, r);
                            }
                        }
                    }
                }
            }
        } else {
            log::info!("[{}] Exiting event loop", tag);
        }
    }
}
