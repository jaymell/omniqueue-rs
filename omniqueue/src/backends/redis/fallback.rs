//! Implementation of the main queue using two lists instead of redis streams,
//! for compatibility with redis versions older than 6.2.0.

use std::time::Duration;

use bb8::ManageConnection;
use redis::AsyncCommands;
use svix_ksuid::{KsuidLike as _, KsuidMs};
use time::OffsetDateTime;
use tracing::{error, trace};

use super::{
    internal_from_list, internal_to_list_payload, InternalPayloadOwned, RawPayload,
    RedisConnection, RedisConsumer, RedisProducer,
};
use crate::{queue::Acker, Delivery, QueueError, Result};

pub(super) async fn send_raw<R: RedisConnection>(
    producer: &RedisProducer<R>,
    payload: &[u8],
) -> Result<()> {
    producer
        .redis
        .get()
        .await
        .map_err(QueueError::generic)?
        .lpush(&producer.queue_key, internal_to_list_payload((payload, 0)))
        .await
        .map_err(QueueError::generic)
}

pub(super) async fn receive<R: RedisConnection>(consumer: &RedisConsumer<R>) -> Result<Delivery> {
    let res = receive_with_timeout(consumer, Duration::ZERO).await?;
    res.ok_or_else(|| QueueError::Generic("No data".into()))
}

pub(super) async fn receive_all<R: RedisConnection>(
    consumer: &RedisConsumer<R>,
    deadline: Duration,
    _max_messages: usize,
) -> Result<Vec<Delivery>> {
    // FIXME: Run up to max_messages RPOPLPUSH'es until there is a null reply?
    let delivery = receive_with_timeout(consumer, deadline).await?;
    Ok(delivery.into_iter().collect())
}

async fn receive_with_timeout<R: RedisConnection>(
    consumer: &RedisConsumer<R>,
    timeout: Duration,
) -> Result<Option<Delivery>> {
    let payload: Option<Vec<u8>> = consumer
        .redis
        .get()
        .await
        .map_err(QueueError::generic)?
        .brpoplpush(
            &consumer.queue_key,
            &consumer.processing_queue_key,
            // The documentation at https://redis.io/docs/latest/commands/brpoplpush/ does not
            // state what unit the timeout is, but `BLPOP` and `BLMPOP` have similar timeout
            // parameters that are documented as being seconds.
            timeout.as_secs_f64(),
        )
        .await
        .map_err(QueueError::generic)?;

    match payload {
        Some(old_payload) => {
            let (payload, num_receives) = internal_from_list(&old_payload)?;
            Some(internal_to_delivery(
                (payload.to_vec(), num_receives),
                consumer,
                old_payload,
            ))
            .transpose()
        }
        None => Ok(None),
    }
}

fn internal_to_delivery<R: RedisConnection>(
    internal: InternalPayloadOwned,
    consumer: &RedisConsumer<R>,
    old_payload: Vec<u8>,
) -> Result<Delivery> {
    let (payload, num_receives) = internal;
    Ok(Delivery::new(
        payload,
        RedisFallbackAcker {
            redis: consumer.redis.clone(),
            processing_queue_key: consumer.processing_queue_key.clone(),
            old_payload,
            already_acked_or_nacked: false,
            max_receives: consumer.max_receives,
            num_receives,
        },
    ))
}

pub(super) struct RedisFallbackAcker<M: ManageConnection> {
    pub(super) redis: bb8::Pool<M>,
    pub(super) processing_queue_key: String,
    // We delete based on the payload -- and since the
    // `num_receives` changes after receiving it's the
    // `old_payload`, since `num_receives` is part of the
    // payload. Make sense?
    pub(super) old_payload: RawPayload,

    pub(super) already_acked_or_nacked: bool,

    pub(super) max_receives: usize,
    pub(super) num_receives: usize,
}

impl<R: RedisConnection> Acker for RedisFallbackAcker<R> {
    async fn ack(&mut self) -> Result<()> {
        if self.already_acked_or_nacked {
            return Err(QueueError::CannotAckOrNackTwice);
        }

        let _: () = self
            .redis
            .get()
            .await
            .map_err(QueueError::generic)?
            .lrem(&self.processing_queue_key, 1, &self.old_payload)
            .await
            .map_err(QueueError::generic)?;

        self.already_acked_or_nacked = true;

        Ok(())
    }

    async fn nack(&mut self) -> Result<()> {
        if self.num_receives >= self.max_receives {
            trace!("Maximum attempts reached");
            return self.ack().await;
        }

        if self.already_acked_or_nacked {
            return Err(QueueError::CannotAckOrNackTwice);
        }

        self.already_acked_or_nacked = true;

        Ok(())
    }

    async fn set_ack_deadline(&mut self, _duration: Duration) -> Result<()> {
        Err(QueueError::Unsupported(
            "set_ack_deadline is not yet supported by redis fallback backend",
        ))
    }
}

pub(super) async fn add_to_main_queue(
    keys: &[Vec<u8>],
    main_queue_name: &str,
    conn: &mut impl AsyncCommands,
) -> Result<()> {
    let new_keys = keys
        .iter()
        .map(|x| regenerate_key(x))
        .collect::<Result<Vec<_>>>()?;
    let _: () = conn
        .lpush(main_queue_name, new_keys)
        .await
        .map_err(QueueError::generic)?;
    Ok(())
}

pub(super) async fn background_task_processing<R: RedisConnection>(
    pool: bb8::Pool<R>,
    queue_key: String,
    processing_queue_key: String,
    ack_deadline_ms: i64,
    max_receives: usize,
) -> Result<()> {
    // FIXME: ack_deadline_ms should be unsigned
    let ack_deadline = Duration::from_millis(ack_deadline_ms as _);
    loop {
        if let Err(err) = reenqueue_timed_out_messages(
            &pool,
            &queue_key,
            &processing_queue_key,
            ack_deadline,
            max_receives,
        )
        .await
        {
            error!("{err}");
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
    }
}

async fn reenqueue_timed_out_messages<R: RedisConnection>(
    pool: &bb8::Pool<R>,
    queue_key: &str,
    processing_queue_key: &str,
    ack_deadline: Duration,
    max_receives: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const BATCH_SIZE: isize = 50;

    let mut conn = pool.get().await?;

    let keys: Vec<RawPayload> = conn.lrange(processing_queue_key, 0, 1).await?;

    // If the key is older than now, it means we should be processing keys
    let validity_limit = KsuidMs::new(Some(OffsetDateTime::now_utc() - ack_deadline), None)
        .to_string()
        .into_bytes();

    if !keys.is_empty() && keys[0] <= validity_limit {
        let keys: Vec<RawPayload> = conn.lrange(processing_queue_key, 0, BATCH_SIZE).await?;
        for key in keys {
            if key <= validity_limit {
                let internal = internal_from_list(&key)?;
                let num_receives = internal.1;
                if num_receives >= max_receives {
                    trace!(
                        num_receives = num_receives,
                        "Maximum attempts reached for message, not reenqueuing",
                    );
                } else {
                    trace!(
                        num_receives = num_receives,
                        "Pushing back overdue task to queue"
                    );
                    let _: () = conn
                        .rpush(queue_key, internal_to_list_payload(internal))
                        .await?;
                }

                // We use LREM to be sure we only delete the keys we should be deleting
                let _: () = conn.lrem(processing_queue_key, 1, &key).await?;
            }
        }
    } else {
        // Sleep before attempting to fetch again if nothing was found
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}

fn regenerate_key(key: &[u8]) -> Result<RawPayload> {
    Ok(internal_to_list_payload(internal_from_list(key)?))
}
