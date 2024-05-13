use super::blocking;
#[cfg(feature = "nonblocking")]
use super::nonblocking;
use super::{level::DataDogLogLevel, log::DataDogLog};
#[cfg(feature = "nonblocking")]
use crate::client::AsyncDataDogClient;
use crate::{client::DataDogClient, config::DataDogConfig, error::DataDogLoggerError};
use flume::{bounded, unbounded, Receiver, Sender};
#[cfg(feature = "nonblocking")]
use futures::Future;
use log::{LevelFilter, Log, Metadata, Record};
use std::{collections::HashMap, fmt::Display, ops::Drop, thread};

#[derive(Debug)]
/// Logger that logs directly to DataDog via HTTP(S)
pub struct DataDogLogger {
    config: DataDogConfig,
    logsender: Option<Sender<DataDogLog>>,
    selflogrv: Option<Receiver<String>>,
    selflogsd: Option<Sender<String>>,
    logger_handle: Option<thread::JoinHandle<()>>,
}

impl DataDogLogger {
    /// Exposes self log of the logger.
    ///
    /// Contains diagnostic messages with details of errors occuring inside logger.
    /// It will be `None`, unless `enable_self_log` in [`DataDogConfig`](crate::config::DataDogConfig) is set to `true`.
    pub fn selflog(&self) -> &Option<Receiver<String>> {
        &self.selflogrv
    }

    /// Creates new blocking DataDogLogger instance
    ///
    /// What it means is that no executor is used to host DataDog network client. A new thread is started instead.
    /// It receives messages to log and sends them in batches in blocking fashion.
    /// As this is a separate thread, calling [`log`](Self::log) does not imply any IO operation, thus is quite fast.
    ///
    /// # Examples
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::DataDogLogger, client::HttpDataDogClient};
    ///
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///let logger = DataDogLogger::blocking(client, config);
    ///```
    pub fn blocking<T>(client: T, config: DataDogConfig) -> Self
    where
        T: DataDogClient + Send + 'static,
    {
        let (slsender, slreceiver) = if config.enable_self_log {
            let (s, r) = bounded::<String>(100);
            (Some(s), Some(r))
        } else {
            (None, None)
        };
        let slogsender_clone = slsender.clone();
        let (sender, receiver) = match config.messages_channel_capacity {
            Some(capacity) => bounded(capacity),
            None => unbounded(),
        };

        let logger_handle =
            thread::spawn(move || blocking::logger_thread(client, receiver, slsender));

        DataDogLogger {
            config,
            logsender: Some(sender),
            selflogrv: slreceiver,
            selflogsd: slogsender_clone,
            logger_handle: Some(logger_handle),
        }
    }

    /// Creates new non-blocking `DataDogLogger` instance
    ///
    /// Internally spawns logger future to `tokio` runtime.
    /// It is equivalent to calling [`non_blocking_cold`](Self::non_blocking_cold) and spawning future to Tokio runtime.
    /// Thus it is only a convinience function.
    #[cfg(feature = "with-tokio")]
    pub fn non_blocking_with_tokio<T>(client: T, config: DataDogConfig) -> Self
    where
        T: AsyncDataDogClient + Send + 'static,
    {
        let (logger, future) = Self::non_blocking_cold(client, config);
        tokio::spawn(future);
        logger
    }

    /// Creates new non-blocking `DataDogLogger` instance
    ///
    /// What it means is that logger requires executor to run. This executor will host a task that will receive messages to log.
    /// It will log them using non blocking (asynchronous) implementation of network client.
    ///
    /// It returns a `Future` that needs to be spawned for logger to work. This `Future` is a task that is responsible for sending messages.
    /// Although a little inconvinient, it is completely executor agnostic.
    ///
    /// # Examples
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::DataDogLogger, client::HttpDataDogClient};
    ///
    ///# async fn func() {
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///let (logger, future) = DataDogLogger::non_blocking_cold(client, config);
    ///
    ///tokio::spawn(future);
    ///# }
    ///```
    #[cfg(feature = "nonblocking")]
    pub fn non_blocking_cold<T>(
        client: T,
        config: DataDogConfig,
    ) -> (Self, impl Future<Output = ()>)
    where
        T: AsyncDataDogClient,
    {
        let (slsender, slreceiver) = if config.enable_self_log {
            let (s, r) = bounded::<String>(100);
            (Some(s), Some(r))
        } else {
            (None, None)
        };
        let slogsender_clone = slsender.clone();
        let (logsender, logreceiver) = match config.messages_channel_capacity {
            Some(capacity) => bounded(capacity),
            None => unbounded(),
        };
        let logger_future = nonblocking::logger_future(client, logreceiver, slsender);

        let logger = DataDogLogger {
            config,
            logsender: Some(logsender),
            selflogrv: slreceiver,
            selflogsd: slogsender_clone,
            logger_handle: None,
        };

        (logger, logger_future)
    }

    /// Sends log to DataDog thread or task.
    ///
    /// This function does not invoke any IO operation by itself. Instead it sends messages to logger thread or task using channels.
    /// Therefore it is quite lightweight.
    ///
    /// ## Examples
    ///
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::{DataDogLogger, DataDogLogLevel}, client::HttpDataDogClient};
    ///
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///let logger = DataDogLogger::blocking(client, config);
    ///
    ///logger.log("message", DataDogLogLevel::Error);
    ///```
    pub fn log<T: Display>(&self, message: T, level: DataDogLogLevel) {
        let log = DataDogLog {
            message: message.to_string(),
            ddtags: self.config.tags.clone(),
            service: self.config.service.clone().unwrap_or_default(),
            host: self.config.hostname.clone().unwrap_or_default(),
            ddsource: self.config.source.clone(),
            level: level.to_string(),
        };

        if let Some(ref sender) = self.logsender {
            match sender.try_send(log) {
                Ok(()) => {
                    // nothing
                }
                Err(e) => {
                    if let Some(ref selflog) = self.selflogsd {
                        selflog.try_send(e.to_string()).unwrap_or_default();
                    }
                }
            }
        }
    }

    /// Sends log to DataDog thread or task.
    ///
    /// This function does not invoke any IO operation by itself. Instead it sends messages to logger thread or task using channels.
    /// Therefore it is quite lightweight.
    ///
    /// ## Examples
    ///
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::{DataDogLogger, DataDogLogLevel}, client::HttpDataDogClient};
    ///
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///let logger = DataDogLogger::blocking(client, config);
    ///
    ///let tags = {
    ///  let mut tags = std::collections::HashMap::new();
    ///  tags.insert("tag1".to_string(), "value1".to_string());
    ///  tags.insert("tag2".to_string(), "value2".to_string());
    ///  tags.insert("block_number".to_string(), 23123423.to_string());
    ///  tags
    ///};
    ///logger.log_with_tags("message", tags, DataDogLogLevel::Error);
    ///```
    pub fn log_with_tags<T: Display>(
        &self,
        message: T,
        tags: HashMap<String, String>,
        level: DataDogLogLevel,
    ) {
        let mut log = DataDogLog {
            message: message.to_string(),
            ddtags: self.config.tags.clone(),
            service: self.config.service.clone().unwrap_or_default(),
            host: self.config.hostname.clone().unwrap_or_default(),
            ddsource: self.config.source.clone(),
            level: level.to_string(),
        };

        // Format the provided tags and merge with the existing ones
        let formatted_tags = tags
            .into_iter()
            .map(|(key, value)| format!("{}:{}", key, value))
            .collect::<Vec<String>>()
            .join(" ");

        if let Some(existing_tags) = &log.ddtags {
            log.ddtags = Some(format!("{} {}", existing_tags, formatted_tags));
        } else {
            log.ddtags = Some(formatted_tags);
        }

        if let Some(ref sender) = self.logsender {
            match sender.try_send(log) {
                Ok(()) => {
                    // nothing
                }
                Err(e) => {
                    if let Some(ref selflog) = self.selflogsd {
                        selflog.try_send(e.to_string()).unwrap_or_default();
                    }
                }
            }
        }
    }

    /// Initializes blocking DataDogLogger with `log` crate.
    /// # Examples
    ///
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::{DataDogLogger, DataDogLogLevel}, client::HttpDataDogClient};
    ///use log::*;
    ///
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///
    ///DataDogLogger::set_blocking_logger(client, config, LevelFilter::Error);
    ///
    ///error!("An error occured");
    ///warn!("A warning")
    ///```
    pub fn set_blocking_logger<T>(
        client: T,
        config: DataDogConfig,
        level: LevelFilter,
    ) -> Result<(), DataDogLoggerError>
    where
        T: DataDogClient + Send + 'static,
    {
        let logger = DataDogLogger::blocking(client, config);
        log::set_boxed_logger(Box::new(logger))?;
        log::set_max_level(level);
        Ok(())
    }

    /// Initializes nonblocking DataDogLogger with `log` crate.
    ///
    /// To make logger work, returned future has to be spawned to executor.
    /// # Examples
    ///```rust
    ///use datadog_logs::{config::DataDogConfig, logger::DataDogLogger, client::HttpDataDogClient};
    ///use log::*;
    ///
    ///# async fn func() {
    ///let config = DataDogConfig::default();
    ///let client = HttpDataDogClient::new(&config).unwrap();
    ///let future = DataDogLogger::set_nonblocking_logger(client, config, LevelFilter::Error).unwrap();
    ///
    ///tokio::spawn(future);
    ///
    ///error!("An error occured");
    ///warn!("A warning");
    ///# }
    ///```
    #[cfg(feature = "nonblocking")]
    pub fn set_nonblocking_logger<T>(
        client: T,
        config: DataDogConfig,
        level: LevelFilter,
    ) -> Result<impl Future<Output = ()>, DataDogLoggerError>
    where
        T: AsyncDataDogClient + Send + 'static,
    {
        let (logger, future) = DataDogLogger::non_blocking_cold(client, config);
        log::set_boxed_logger(Box::new(logger))?;
        log::set_max_level(level);
        Ok(future)
    }
}

impl Log for DataDogLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let level = match record.level() {
            log::Level::Error => DataDogLogLevel::Error,
            log::Level::Warn => DataDogLogLevel::Warning,
            log::Level::Info => DataDogLogLevel::Informational,
            log::Level::Debug | log::Level::Trace => DataDogLogLevel::Debug,
        };

        &self.log(format!("{}", record.args()), level);
    }

    fn flush(&self) {}
}

impl Drop for DataDogLogger {
    fn drop(&mut self) {
        // drop sender to allow logger thread to close
        std::mem::drop(self.logsender.take());

        // wait for logger thread to finish to ensure all messages are flushed
        if let Some(handle) = self.logger_handle.take() {
            handle.join().unwrap_or_default();
        }
    }
}
