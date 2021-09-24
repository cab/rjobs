use backends::Backend;
use chrono::{DateTime, Duration, Utc};
use nanoid::nanoid;
use redis::{AsyncCommands, FromRedisValue, RedisWrite, ToRedisArgs};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::sync::Arc;
use std::{collections::VecDeque, num::NonZeroUsize};
use thiserror::private::AsDynError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, error, info, warn};

pub use backends::{memory::MemoryBackend, redis::RedisBackend};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
    #[error(transparent)]
    Serialization(#[from] bincode::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Schedulable: Serialize + DeserializeOwned {
    type Error;

    fn perform(&mut self) -> std::result::Result<(), Self::Error>;
}

#[derive(Debug)]
pub struct Job {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobId(String);

impl JobId {
    fn random() -> Self {
        Self(nanoid!())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct JobDefinition {
    serialized_job: Vec<u8>,
    id: JobId,
    enqueued_at: DateTime<Utc>,
    #[serde(skip)]
    debug: Option<JobDefinitionDebug>,
}

impl std::fmt::Debug for JobDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobDefinition")
            .field("id", &self.id)
            .field("enqueued_at", &self.enqueued_at)
            .field("debug", &self.debug)
            .finish()
    }
}

#[derive(Debug, Clone)]
struct JobDefinitionDebug {
    job_type_name: &'static str,
}

impl JobDefinitionDebug {
    fn new<T>() -> Self {
        Self {
            job_type_name: std::any::type_name::<T>(),
        }
    }
}

impl FromRedisValue for JobDefinition {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        let bytes = <Vec<u8> as FromRedisValue>::from_redis_value(v)?;
        let def = bincode::deserialize::<Self>(&bytes)
            .map_err(|e| (redis::ErrorKind::TypeError, "bincode failed"))?;
        Ok(def)
    }
}

impl JobDefinition {
    fn new<S>(job: &S, enqueued_at: DateTime<Utc>) -> Result<Self>
    where
        S: Serialize,
    {
        let id = JobId::random();
        let serialized_job = bincode::serialize(job)?;
        Ok(Self {
            serialized_job,
            id,
            enqueued_at,
            debug: Some(JobDefinitionDebug::new::<S>()),
        })
    }

    fn to_redis_args(&self) -> Result<impl ToRedisArgs> {
        let bytes = bincode::serialize(self)?;
        Ok(bytes)
    }
}

#[derive(Debug)]
struct Queue<Backend> {
    name: QueueName,
    jobs: VecDeque<JobDefinition>,
    backend: Backend,
}

impl<Backend> Queue<Backend>
where
    Backend: backends::Backend,
{
    fn new(name: QueueName, backend: Backend) -> Self {
        Self {
            name,
            backend,
            jobs: VecDeque::new(),
        }
    }

    pub(crate) async fn process(&mut self) -> Result<()> {
        let max_pending_jobs = 3; //todo configurable
        if self.jobs.len() < max_pending_jobs {
            self.pull(NonZeroUsize::new(max_pending_jobs).unwrap())
                .await?;
        }
        self.run_next_job().await?;
        Ok(())
    }

    async fn run_next_job(&mut self) -> Result<()> {
        if let Some(next_job) = self.jobs.pop_front() {
            info!("running job: {:?}", next_job);
        }
        Ok(())
    }

    pub(crate) async fn drain(&mut self, from_backend: bool) -> Result<()> {
        info!("draining {}", self.name);
        if from_backend {
            loop {
                match self.pull(NonZeroUsize::new(100).unwrap()).await {
                    Err(e) => {
                        warn!("failed to drain from backend: {}", e);
                        break;
                    }
                    Ok(size) => {
                        if size == 0 {
                            break;
                        }
                    }
                }
            }
        }
        while !self.jobs.is_empty() {
            if let Err(e) = self.run_next_job().await {
                warn!("job failed: {}", e);
            }
        }
        Ok(())
    }

    async fn pull(&mut self, count: NonZeroUsize) -> Result<usize> {
        let job_defs = self.backend.pull(&self.name, count).await?;
        let count = job_defs.len();
        debug!("pulled {} jobs", count);
        self.jobs.append(&mut VecDeque::from(job_defs));
        Ok(count)
    }
}

mod backends {
    use std::num::NonZeroUsize;

    use crate::{Error, JobDefinition, QueueName, Result};
    #[async_trait::async_trait]
    pub trait Backend: Clone + Send + Sync {
        async fn schedule(&self, queue: &QueueName, job_def: &JobDefinition) -> Result<()>;
        async fn pull(&self, queue: &QueueName, count: NonZeroUsize) -> Result<Vec<JobDefinition>>;
    }

    pub(crate) mod memory {
        use std::{
            cell::RefCell,
            collections::{HashMap, VecDeque},
            num::NonZeroUsize,
            sync::{Arc, Mutex},
        };

        use tracing::debug;

        use crate::{Error, JobDefinition, QueueName, Result};

        #[derive(Debug, Clone)]
        pub struct MemoryBackend {
            jobs_by_queue: Arc<Mutex<RefCell<HashMap<QueueName, VecDeque<JobDefinition>>>>>,
        }

        impl MemoryBackend {
            pub fn new() -> Self {
                Self {
                    jobs_by_queue: Arc::new(Mutex::new(RefCell::new(HashMap::new()))),
                }
            }
        }

        #[async_trait::async_trait]
        impl super::Backend for MemoryBackend {
            async fn pull(
                &self,
                queue: &QueueName,
                count: NonZeroUsize,
            ) -> Result<Vec<JobDefinition>> {
                if let Some(values) = self
                    .jobs_by_queue
                    .lock()
                    .unwrap() // todo
                    .borrow_mut()
                    .get_mut(queue)
                {
                    let max = std::cmp::min(values.len(), count.get());
                    let jobs = values.drain(0..max);
                    Ok(jobs.into_iter().collect())
                } else {
                    Ok(vec![])
                }
            }

            async fn schedule(&self, queue: &QueueName, job_def: &JobDefinition) -> Result<()> {
                self.jobs_by_queue
                    .lock()
                    .unwrap() // todo
                    .borrow_mut()
                    .entry(queue.clone())
                    .or_insert_with(VecDeque::new)
                    .push_back(job_def.clone());
                Ok(())
            }
        }
    }

    pub(crate) mod redis {
        use std::num::NonZeroUsize;

        use redis::AsyncCommands;
        use tracing::warn;

        use crate::{Error, JobDefinition, Queue, QueueName, Result};

        #[derive(Debug, Clone)]
        pub struct RedisBackend {
            redis_client: redis::Client,
        }

        impl RedisBackend {
            pub fn new(redis_url: &str) -> Result<Self> {
                let redis_client = redis::Client::open(redis_url)?;
                Ok(Self { redis_client })
            }
        }

        #[async_trait::async_trait]
        impl super::Backend for RedisBackend {
            async fn pull(
                &self,
                queue: &QueueName,
                count: NonZeroUsize,
            ) -> Result<Vec<JobDefinition>> {
                let mut connection = self.redis_client.get_async_connection().await?;
                // let job_defs = connection
                //     .rpop::<_, Vec<JobDefinition>>(queue, Some(count))
                //     .await?;
                let mut job_defs = Vec::new();
                for _ in 0..count.get() {
                    match connection
                        .rpop::<_, Option<JobDefinition>>(queue, None)
                        .await
                    {
                        Ok(Some(job_def)) => job_defs.push(job_def),
                        Ok(None) => {
                            break;
                        }
                        Err(e) => {
                            warn!("failed to rpop: {}", e);
                            break;
                        }
                    }
                }
                Ok(job_defs)
            }

            async fn schedule(&self, queue: &QueueName, job_def: &JobDefinition) -> Result<()> {
                let mut connection = self.redis_client.get_async_connection().await?;
                let () = connection.lpush(&queue, job_def.to_redis_args()?).await?;
                Ok(())
            }
        }
    }
}

pub struct Scheduler<Backend> {
    backend: Backend,
    poller: Poller<Backend>,
    manager: Manager<Backend>,
}

impl<Backend> Scheduler<Backend>
where
    Backend: backends::Backend + 'static,
{
    pub fn new(backend: Backend) -> Result<Self> {
        let poller = Poller::new(backend.clone(), Duration::seconds(1));
        let manager = Manager::new(backend.clone(), Duration::seconds(1));
        Ok(Self {
            backend,
            poller,
            manager,
        })
    }

    pub fn start(&mut self) {
        self.manager.start();
        self.poller.start();
    }

    pub async fn drain(&mut self, from_backend: bool) -> Result<()> {
        if let Err(e) = self.poller.stop().await {
            error!("failed to stop poller: {}", e);
        }
        if let Err(e) = self.manager.drain(from_backend).await {
            error!("failed to stop manager: {}", e);
        }
        Ok(())
    }

    pub async fn schedule(&self, job: impl Schedulable) -> Result<JobId> {
        let queue = QueueName::from("default");
        let job_def = JobDefinition::new(&job, chrono::Utc::now())?;
        debug!("scheduling {:?} on {}", job_def, queue);
        self.backend.schedule(&queue, &job_def).await?;
        Ok(job_def.id)
    }
}

#[derive(Debug)]
struct Manager<Backend> {
    rate: Duration,
    job_comms: (UnboundedSender<()>, UnboundedReceiver<()>),
    handle_comms: (UnboundedSender<()>, Option<UnboundedReceiver<()>>),
    backend: Backend,
    timer_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    inner: Arc<RwLock<ManagerInner<Backend>>>,
}

#[derive(Debug)]
struct ManagerInner<Backend> {
    queues: Vec<Queue<Backend>>,
}

impl<Backend> Manager<Backend>
where
    Backend: backends::Backend + 'static,
{
    fn new(backend: Backend, rate: Duration) -> Self {
        let job_comms = unbounded_channel();
        let handle_comms = unbounded_channel();
        let queues = vec![Queue::new(QueueName::from("default"), backend.clone())];
        Self {
            timer_handle: None,
            handle_comms: (handle_comms.0, Some(handle_comms.1)),
            inner: Arc::new(RwLock::new(ManagerInner { queues })),
            rate,
            job_comms,
            backend,
        }
    }

    async fn drain(&mut self, from_backend: bool) -> Result<()> {
        debug!("sending message to drain manager");
        self.handle_comms.0.send(()).unwrap(); // todo handle error
        for queue in &mut self.inner.write().await.queues {
            if let Err(e) = queue.drain(from_backend).await {
                warn!("failed to drain `{}`: {}", queue.name, e);
            }
        }
        if let Some(handle) = self.timer_handle.take() {
            let output = handle.await.unwrap(); // TODO handle error
            if let Err(e) = output {
                warn!("manager errored TODO");
            }
        }
        Ok(())
    }

    fn start(&mut self) {
        if self.timer_handle.is_some() {
            warn!("already started");
            return;
        }
        self.timer_handle = Some(tokio::spawn({
            let mut rx = self.handle_comms.1.take().unwrap();
            let tx = self.job_comms.0.clone();
            let rate = self.rate.clone();
            let inner = self.inner.clone();
            async move {
                let mut interval = time::interval(rate.to_std().unwrap());
                loop {
                    if let Ok(task_message) = rx.try_recv() {
                        info!("manager stopping");
                        // todo multiple types of message
                        break;
                    }
                    interval.tick().await;

                    let mut inner = inner.write().await;
                    for queue in &mut inner.queues {
                        queue.process().await; // todo handle error
                    }

                    if let Err(e) = tx.send(()) {
                        warn!("failed to send, todo");
                    }
                }
                Result::Ok(())
            }
        }));
    }
}

#[derive(Debug)]
struct Poller<Backend> {
    rate: Duration,
    backend: Backend,
    job_comms: (UnboundedSender<()>, UnboundedReceiver<()>),
    handle_comms: (UnboundedSender<()>, Option<UnboundedReceiver<()>>),
    timer_handle: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl<Backend> Poller<Backend>
where
    Backend: backends::Backend + 'static,
{
    fn new(backend: Backend, rate: Duration) -> Self {
        let job_comms = unbounded_channel();
        let handle_comms = unbounded_channel();
        Self {
            handle_comms: (handle_comms.0, Some(handle_comms.1)),
            timer_handle: None,
            backend,
            job_comms,
            rate,
        }
    }

    async fn stop(&mut self) -> Result<()> {
        debug!("sending message to stop poller");
        self.handle_comms.0.send(()).unwrap(); // todo handle error
        if let Some(handle) = self.timer_handle.take() {
            let output = handle.await.unwrap(); // TODO handle error
            if let Err(e) = output {
                warn!("poller errored TODO");
            }
        }
        Ok(())
    }

    fn start(&mut self) {
        if self.timer_handle.is_some() {
            warn!("already started");
            return;
        }
        self.timer_handle = Some(tokio::spawn({
            let mut rx = self.handle_comms.1.take().unwrap();
            let tx = self.job_comms.0.clone();
            let rate = self.rate.clone();
            async move {
                let mut interval = time::interval(rate.to_std().unwrap());
                loop {
                    if let Ok(task_message) = rx.try_recv() {
                        info!("poller stopping");
                        // todo multiple types of message
                        break;
                    }
                    interval.tick().await;
                    if let Err(e) = tx.send(()) {
                        warn!("failed to send, todo");
                    }
                }
                Result::Ok(())
            }
        }));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueName(String);

impl std::fmt::Display for QueueName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("queue:{}", self.0))
    }
}

impl<S> From<S> for QueueName
where
    S: Into<String>,
{
    fn from(s: S) -> Self {
        Self(s.into())
    }
}

impl QueueName {}

impl ToRedisArgs for QueueName {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + RedisWrite,
    {
        let key = format!("queue:{}", self.0);
        out.write_arg(key.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
