use crate::{
    decoding::DecoderRegistry,
    encoding::{CustomEncoder, EncoderRegistry},
    queue::{consumer::QueueConsumer, producer::QueueProducer, Acker, Delivery, QueueBackend},
    QueueError,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use google_cloud_googleapis::pubsub::v1::PubsubMessage;
use google_cloud_pubsub::client::{
    google_cloud_auth::credentials::CredentialsFile, Client, ClientConfig,
};
use google_cloud_pubsub::subscriber::ReceivedMessage;
use google_cloud_pubsub::subscription::Subscription;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{any::TypeId, collections::HashMap};

pub struct GcpPubSubBackend;

type Payload = Vec<u8>;
type Encoders = EncoderRegistry<Payload>;
type Decoders = DecoderRegistry<Payload>;

// FIXME: topic/subscription are each for read/write. Split config up?
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcpPubSubConfig {
    pub topic_id: String,
    pub subscription_id: String,
    pub credentials_file: Option<PathBuf>,
}

/// Make a `ClientConfig` from a `CredentialsFile` on disk.
async fn configure_client_from_file<P: AsRef<Path>>(
    cred_file_path: P,
) -> Result<ClientConfig, QueueError> {
    let bytes = std::fs::read(cred_file_path).map_err(QueueError::generic)?;
    let creds: CredentialsFile = serde_json::from_slice(&bytes).map_err(QueueError::generic)?;
    ClientConfig::default()
        .with_credentials(creds)
        .await
        .map_err(QueueError::generic)
}

/// Making a `ClientConfig` via env vars is possible in two ways:
/// - setting `GOOGLE_APPLICATION_CREDENTIALS` to the file path to have it loaded automatically
/// - setting `GOOGLE_APPLICATION_CREDENTIALS_JSON` to the file contents (avoiding the need for a
///   file on disk).
async fn configure_client_from_env() -> Result<ClientConfig, QueueError> {
    ClientConfig::default()
        .with_auth()
        .await
        .map_err(QueueError::generic)
}

async fn get_client(cfg: &GcpPubSubConfig) -> Result<Client, QueueError> {
    let config = {
        if let Some(fp) = &cfg.credentials_file {
            tracing::trace!("reading gcp creds from file: {}", fp.display());
            configure_client_from_file(&fp).await?
        } else {
            tracing::trace!("reading gcp creds from env");
            configure_client_from_env().await?
        }
    };
    Client::new(config).await.map_err(QueueError::generic)
}

impl GcpPubSubConsumer {
    async fn new(
        client: Client,
        subscription_id: String,
        registry: Decoders,
    ) -> Result<Self, QueueError> {
        Ok(Self {
            client,
            registry,
            subscription_id: Arc::new(subscription_id),
        })
    }
}

impl GcpPubSubProducer {
    async fn new(client: Client, topic_id: String, registry: Encoders) -> Result<Self, QueueError> {
        let topic = client.topic(&topic_id);
        // Only warn if the topic doesn't exist at this point.
        // If it gets created after the fact, we should be able to still use it when available,
        // otherwise if it's still missing at that time, error.
        if !topic.exists(None).await.map_err(QueueError::generic)? {
            tracing::warn!("topic {} does not exist", &topic_id);
        }
        Ok(Self {
            client,
            registry,
            topic_id: Arc::new(topic_id),
        })
    }
}

#[async_trait]
impl QueueBackend for GcpPubSubBackend {
    type Config = GcpPubSubConfig;

    type PayloadIn = Payload;
    type PayloadOut = Payload;

    type Producer = GcpPubSubProducer;
    type Consumer = GcpPubSubConsumer;

    async fn new_pair(
        config: Self::Config,
        custom_encoders: Encoders,
        custom_decoders: Decoders,
    ) -> Result<(GcpPubSubProducer, GcpPubSubConsumer), QueueError> {
        let client = get_client(&config).await?;
        Ok((
            GcpPubSubProducer::new(client.clone(), config.topic_id, custom_encoders).await?,
            GcpPubSubConsumer::new(client, config.subscription_id, custom_decoders).await?,
        ))
    }

    async fn producing_half(
        config: Self::Config,
        custom_encoders: EncoderRegistry<Self::PayloadIn>,
    ) -> Result<GcpPubSubProducer, QueueError> {
        let client = get_client(&config).await?;
        GcpPubSubProducer::new(client, config.topic_id, custom_encoders).await
    }

    async fn consuming_half(
        config: Self::Config,
        custom_decoders: DecoderRegistry<Self::PayloadOut>,
    ) -> Result<GcpPubSubConsumer, QueueError> {
        let client = get_client(&config).await?;
        GcpPubSubConsumer::new(client, config.subscription_id, custom_decoders).await
    }
}

pub struct GcpPubSubProducer {
    client: Client,
    registry: Encoders,
    topic_id: Arc<String>,
}

impl std::fmt::Debug for GcpPubSubProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("GcpPubSubProducer")
            .field("topic_id", &self.topic_id)
            .finish()
    }
}

#[async_trait]
impl QueueProducer for GcpPubSubProducer {
    type Payload = Payload;

    fn get_custom_encoders(&self) -> &HashMap<TypeId, Box<dyn CustomEncoder<Self::Payload>>> {
        self.registry.as_ref()
    }

    async fn send_raw(&self, payload: &Self::Payload) -> Result<(), QueueError> {
        let msg = PubsubMessage {
            data: payload.to_vec(),
            ..Default::default()
        };

        // N.b. defer the creation of a publisher/topic until needed. Helps recover when
        // the topic does not yet exist, but will soon.
        // Might be more expensive to recreate each time, but overall more reliable.
        let topic = self.client.topic(&self.topic_id);

        // Publishing to a non-existent topic will cause the publisher to wait (forever?)
        // Giving this error will allow dependents to handle the error case immediately when this
        // happens, instead of holding the connection open indefinitely.
        if !topic.exists(None).await.map_err(QueueError::generic)? {
            return Err(QueueError::Generic(
                format!("topic {} does not exist", &self.topic_id).into(),
            ));
        }
        // FIXME: may need to expose `PublisherConfig` to caller so they can tweak this
        let publisher = topic.new_publisher(None);
        let awaiter = publisher.publish(msg).await;
        awaiter.get().await.map_err(QueueError::generic)?;
        Ok(())
    }

    async fn send_serde_json<P: Serialize + Sync>(&self, payload: &P) -> Result<(), QueueError> {
        self.send_raw(&serde_json::to_vec(&payload)?).await
    }
}

pub struct GcpPubSubConsumer {
    client: Client,
    registry: Decoders,
    subscription_id: Arc<String>,
}
impl std::fmt::Debug for GcpPubSubConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("GcpPubSubConsumer")
            .field("subscription_id", &self.subscription_id)
            .finish()
    }
}

async fn subscription(client: &Client, subscription_id: &str) -> Result<Subscription, QueueError> {
    let subscription = client.subscription(subscription_id);
    if !subscription
        .exists(None)
        .await
        .map_err(QueueError::generic)?
    {
        return Err(QueueError::Generic(
            format!("subscription {} does not exist", &subscription_id).into(),
        ));
    }
    Ok(subscription)
}

impl GcpPubSubConsumer {
    fn wrap_recv_msg(&self, mut recv_msg: ReceivedMessage) -> Delivery {
        // FIXME: would be nice to avoid having to move the data out here.
        //   While it's possible to ack via a subscription and an ack_id, nack is only
        //   possible via a `ReceiveMessage`. This means we either need to hold 2 copies of
        //   the payload, or move the bytes out so they can be returned _outside of the Acker_.
        let payload = recv_msg.message.data.drain(..).collect();

        Delivery {
            decoders: self.registry.clone(),
            acker: Box::new(GcpPubSubAcker {
                recv_msg,
                subscription_id: self.subscription_id.clone(),
            }),
            payload: Some(payload),
        }
    }
}

#[async_trait]
impl QueueConsumer for GcpPubSubConsumer {
    type Payload = Payload;

    async fn receive(&mut self) -> Result<Delivery, QueueError> {
        let subscription = subscription(&self.client, &self.subscription_id).await?;
        let mut stream = subscription
            .subscribe(None)
            .await
            .map_err(QueueError::generic)?;

        let recv_msg = stream.next().await.ok_or_else(|| QueueError::NoData)?;

        Ok(self.wrap_recv_msg(recv_msg))
    }

    async fn receive_all(
        &mut self,
        max_messages: usize,
        deadline: Duration,
    ) -> Result<Vec<Delivery>, QueueError> {
        let subscription = subscription(&self.client, &self.subscription_id).await?;
        match tokio::time::timeout(deadline, subscription.pull(max_messages as _, None)).await {
            Ok(messages) => Ok(messages
                .map_err(QueueError::generic)?
                .into_iter()
                .map(|m| self.wrap_recv_msg(m))
                .collect()),
            // Timeout
            Err(_) => Ok(vec![]),
        }
    }
}

pub struct GcpPubSubAcker {
    recv_msg: ReceivedMessage,
    subscription_id: Arc<String>,
}

impl std::fmt::Debug for GcpPubSubAcker {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("GcpPubSubAcker")
            .field("ack_id", &self.recv_msg.ack_id())
            .field("message_id", &self.recv_msg.message.message_id)
            .field("subscription_id", &self.subscription_id)
            .finish()
    }
}

#[async_trait]
impl Acker for GcpPubSubAcker {
    async fn ack(&mut self) -> Result<(), QueueError> {
        self.recv_msg.ack().await.map_err(QueueError::generic)
    }

    async fn nack(&mut self) -> Result<(), QueueError> {
        self.recv_msg.nack().await.map_err(QueueError::generic)
    }
}