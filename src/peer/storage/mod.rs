mod redis_storage;

use crate::types::{self, Address, AddressExt, EnvelopeExt};
use crate::types::{Backlog, Envelope};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bb8_redis::redis;
pub use redis::*;
pub use redis_storage::RedisStorage;
use serde::{Deserialize, Serialize};

// operation against backlog
#[async_trait]
pub trait Storage: Clone + Send + Sync + 'static {
    // track stores some information about the envelope
    // in a backlog used to track replies received related to this
    // envelope. The envelope has to be a request envelope.
    async fn track(&self, uid: &str, ttl: u64, backlog: Backlog) -> Result<()>;

    // gets message with ID. This will retrieve the object
    // from backlog.$id. On success, this can either be None which means
    // there is no message with that ID or the actual message.
    async fn get(&self, uid: &str) -> Result<Option<Backlog>>;

    // pushes the message to local process (msgbus.$cmd) queue.
    // this means the message will be now available to the application
    // to process.
    //
    // KNOWN ISSUE: we should not set TTL on this queue because
    // we are not sure how long the application will take to process
    // all it's queues messages. So this is potentially dangerous. A harmful
    // twin can flood a server memory by sending many large messages to a `cmd`
    // that is not handled by any application.
    //
    // SUGGESTED FIX: instead of setting TTL on the $cmd queue we can limit the length
    // of the queue. So for example, we allow maximum of 500 message to be on this queue
    // after that we need to trim the queue to specific length after push (so drop older messages)
    async fn request(&self, msg: JsonIncomingRequest) -> Result<()>;

    // pushed a json response back to the caller according to his
    // reply queue.
    async fn response(&self, queue: &str, response: JsonIncomingResponse) -> Result<()>;

    // messages waits on either "requests" or "replies" that are available to
    // be send to remote twin.
    async fn messages(&self) -> Result<JsonMessage>;
}

pub enum JsonMessage {
    Request(JsonOutgoingRequest),
    Response(JsonOutgoingResponse),
}

impl From<JsonOutgoingRequest> for JsonMessage {
    fn from(value: JsonOutgoingRequest) -> Self {
        Self::Request(value)
    }
}

impl From<JsonOutgoingResponse> for JsonMessage {
    fn from(value: JsonOutgoingResponse) -> Self {
        Self::Response(value)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonError {
    pub code: u32,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonOutgoingRequest {
    #[serde(rename = "ver")]
    pub version: usize,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(rename = "cmd")]
    pub command: String,
    #[serde(rename = "exp")]
    pub expiration: u64,
    #[serde(rename = "dat")]
    pub data: String,
    #[serde(rename = "tag")]
    pub tags: Option<String>,
    #[serde(rename = "dst")]
    pub destinations: Vec<u32>,
    #[serde(rename = "ret")]
    pub reply_to: String,
    #[serde(rename = "shm")]
    pub schema: Option<String>,
    #[serde(rename = "now")]
    pub timestamp: u64,
}

impl JsonOutgoingRequest {
    /// parts return all the components of this message. this include a backlog
    /// object with all the tracking information, and all envelopes (one for)
    /// each destination. each envelope is already stamped with correct time
    /// stamps, but missing source, and signature information
    /// return (backlog, envelopes, ttl) where ttl is time to live
    /// for the request
    pub fn parts(self) -> Result<(Backlog, Vec<Envelope>, u64)> {
        // create a backlog tracker.
        // that's the part of the request that stays locally
        let mut backlog = types::Backlog::new();
        backlog.reply_to = self.reply_to;
        backlog.reference = self.reference;

        // create an (incomplete) envelope
        let mut request = types::Request::new();

        request.command = self.command;

        let mut env = Envelope::new();
        env.set_plain(base64::decode(self.data).context("invalid data base64 encoding")?);
        env.tags = self.tags;
        env.timestamp = self.timestamp;
        env.expiration = self.expiration;
        env.signature = None;
        env.schema = self.schema;
        env.set_request(request);

        env.stamp();
        let ttl = env.ttl().context("request has expired")?.as_secs();

        let mut envs: Vec<Envelope>;
        if self.destinations.len() == 1 {
            env.destination = Some(self.destinations[0].into()).into();
            envs = vec![env]
        } else {
            envs = Vec::default();
            for dest in self.destinations {
                env.destination = Some(dest.into()).into();
                envs.push(env.clone());
            }
        }

        Ok((backlog, envs, ttl))
    }
}

impl redis::ToRedisArgs for JsonOutgoingRequest {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let bytes = serde_json::to_vec(self).expect("failed to json encode message");
        out.write_arg(&bytes);
    }
}

impl redis::FromRedisValue for JsonOutgoingRequest {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        if let redis::Value::Data(data) = v {
            serde_json::from_slice(data).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "cannot decode a message from json {}",
                    e.to_string(),
                ))
            })
        } else {
            Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "expected a data type from redis",
            )))
        }
    }
}

impl TryFrom<Envelope> for JsonOutgoingRequest {
    type Error = anyhow::Error;
    fn try_from(mut value: Envelope) -> Result<Self, Self::Error> {
        if !value.has_request() {
            anyhow::bail!("envelope doesn't hold a request");
        }
        let req = value.take_request();

        let data = base64::encode(value.plain());
        Ok(JsonOutgoingRequest {
            version: 1,
            reference: Some(value.uid),
            command: req.command,
            expiration: value.expiration,
            data,
            tags: value.tags,
            destinations: vec![value.destination.twin],
            reply_to: String::default(),
            schema: value.schema,
            timestamp: value.timestamp,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonIncomingRequest {
    #[serde(rename = "ver")]
    pub version: usize,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(rename = "src")]
    pub source: String,
    #[serde(rename = "cmd")]
    pub command: String,
    #[serde(rename = "exp")]
    pub expiration: u64,
    #[serde(rename = "dat")]
    pub data: String,
    #[serde(rename = "tag")]
    pub tags: Option<String>,
    #[serde(rename = "ret")]
    pub reply_to: String,
    #[serde(rename = "shm")]
    pub schema: Option<String>,
    #[serde(rename = "now")]
    pub timestamp: u64,
}

impl redis::ToRedisArgs for JsonIncomingRequest {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let bytes = serde_json::to_vec(self).expect("failed to json encode message");
        out.write_arg(&bytes);
    }
}

impl redis::FromRedisValue for JsonIncomingRequest {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        if let redis::Value::Data(data) = v {
            serde_json::from_slice(data).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "cannot decode a message from json {}",
                    e.to_string(),
                ))
            })
        } else {
            Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "expected a data type from redis",
            )))
        }
    }
}

impl TryFrom<Envelope> for JsonIncomingRequest {
    type Error = anyhow::Error;
    fn try_from(mut value: Envelope) -> Result<Self, Self::Error> {
        if !value.has_request() {
            anyhow::bail!("envelope doesn't hold a request");
        }
        let req = value.take_request();
        let data = base64::encode(value.plain());

        Ok(JsonIncomingRequest {
            version: 1,
            reference: Some(value.uid),
            command: req.command,
            expiration: value.expiration,
            data,
            tags: value.tags,
            source: value.source.stringify(),
            reply_to: String::default(),
            schema: value.schema,
            timestamp: value.timestamp,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonOutgoingResponse {
    #[serde(rename = "ver")]
    pub version: usize,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(rename = "dat")]
    pub data: String,
    #[serde(rename = "dst")]
    pub destination: String,
    #[serde(rename = "shm")]
    pub schema: Option<String>,
    #[serde(rename = "now")]
    pub timestamp: u64,
    #[serde(rename = "err")]
    pub error: Option<JsonError>,
}

impl redis::ToRedisArgs for JsonOutgoingResponse {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let bytes = serde_json::to_vec(self).expect("failed to json encode message");
        out.write_arg(&bytes);
    }
}

impl redis::FromRedisValue for JsonOutgoingResponse {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        if let redis::Value::Data(data) = v {
            serde_json::from_slice(data).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "cannot decode a message from json {}",
                    e.to_string(),
                ))
            })
        } else {
            Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "expected a data type from redis",
            )))
        }
    }
}

impl TryFrom<Envelope> for JsonOutgoingResponse {
    type Error = anyhow::Error;
    fn try_from(env: Envelope) -> Result<Self, Self::Error> {
        use types::envelope::Message;

        // message can be only a response or error
        match env.message {
            None => anyhow::bail!("invalid envelope has no message"),
            Some(Message::Request(_)) => anyhow::bail!("invalid envelope has request message"),
            _ => {} //Some(Message::Response(response)) => response,
        };

        let data = base64::encode(env.plain());

        let response = JsonOutgoingResponse {
            version: 1,
            reference: None,
            data,
            destination: env.destination.stringify(),
            timestamp: env.timestamp,
            schema: env.schema,
            error: if let Some(Message::Error(err)) = env.message {
                Some(JsonError {
                    code: err.code,
                    message: err.message,
                })
            } else {
                None
            },
        };

        Ok(response)
    }
}

impl TryFrom<JsonOutgoingResponse> for Envelope {
    type Error = anyhow::Error;

    fn try_from(value: JsonOutgoingResponse) -> Result<Self, Self::Error> {
        let mut env = Envelope::new();

        match value.error {
            None => {
                env.set_plain(base64::decode(value.data)?);
                env.set_response(types::Response::new());
            }
            Some(e) => {
                let mut err = types::Error::new();
                err.code = e.code;
                err.message = e.message;
                env.set_error(err);
            }
        }

        env.uid = value.reference.unwrap_or_default();
        env.timestamp = value.timestamp;
        env.expiration = 3600; // a response has a fixed timeout
        env.destination = Some(
            Address::from_string(value.destination)
                .context("failed to parse destination address")?,
        )
        .into();
        env.schema = value.schema;

        Ok(env)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JsonIncomingResponse {
    #[serde(rename = "ver")]
    pub version: usize,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    #[serde(rename = "dat")]
    pub data: String,
    #[serde(rename = "src")]
    pub source: String,
    #[serde(rename = "shm")]
    pub schema: Option<String>,
    #[serde(rename = "now")]
    pub timestamp: u64,
    #[serde(rename = "err")]
    pub error: Option<JsonError>,
}

impl redis::ToRedisArgs for JsonIncomingResponse {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let bytes = serde_json::to_vec(self).expect("failed to json encode message");
        out.write_arg(&bytes);
    }
}

impl redis::FromRedisValue for JsonIncomingResponse {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        if let redis::Value::Data(data) = v {
            serde_json::from_slice(data).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "cannot decode a message from json {}",
                    e.to_string(),
                ))
            })
        } else {
            Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "expected a data type from redis",
            )))
        }
    }
}

impl TryFrom<Envelope> for JsonIncomingResponse {
    type Error = anyhow::Error;
    fn try_from(env: Envelope) -> Result<Self, Self::Error> {
        use types::envelope::Message;

        // message can be only a response or error
        match env.message {
            None => anyhow::bail!("invalid envelope has no message"),
            Some(Message::Request(_)) => anyhow::bail!("invalid envelope has request message"),
            _ => {} //Some(Message::Response(response)) => response,
        };

        let data = base64::encode(env.plain());

        let response = JsonIncomingResponse {
            version: 1,
            reference: None,
            data,
            source: env.source.stringify(),
            timestamp: env.timestamp,
            schema: env.schema,
            error: if let Some(Message::Error(err)) = env.message {
                Some(JsonError {
                    code: err.code,
                    message: err.message,
                })
            } else {
                None
            },
        };

        Ok(response)
    }
}
