use crate::{
    error::{Error, Result},
    scheduler::QueueName,
};
use chrono::{DateTime, Utc};
use nanoid::nanoid;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[async_trait::async_trait]
pub trait Schedulable: Send + Sync + Default {
    const NAME: &'static str;
    type Arg: prost::Message + Default;
    type Error: std::fmt::Debug;
    // : Serialize + DeserializeOwned {
    async fn perform(&mut self, arg: Self::Arg) -> std::result::Result<(), Self::Error>;
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
    pub(crate) id: JobId,
    pub(crate) serialized_job_data: Vec<u8>,
    pub(crate) job_name: String,
    enqueued_at: DateTime<Utc>,
    pub(crate) queue: QueueName,
    #[serde(skip)]
    debug: Option<JobDefinitionDebug>,
}

impl std::fmt::Debug for JobDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobDefinition")
            .field("id", &self.id)
            .field("job_name", &self.job_name)
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

impl JobDefinition {
    pub(crate) fn new<S>(
        job_data: &S,
        job_name: String,
        queue: QueueName,
        enqueued_at: DateTime<Utc>,
    ) -> Result<Self>
    where
        S: prost::Message,
    {
        let id = JobId::random();
        let serialized_job_data = job_data.encode_to_vec();
        Ok(Self {
            serialized_job_data,
            job_name,
            queue,
            id,
            enqueued_at,
            debug: Some(JobDefinitionDebug::new::<S>()),
        })
    }
}
