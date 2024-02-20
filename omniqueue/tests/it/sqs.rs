use aws_sdk_sqs::Client;
use omniqueue::{
    backends::sqs::{SqsBackend, SqsConfig},
    queue::{consumer::QueueConsumer, producer::QueueProducer, QueueBackend, QueueBuilder, Static},
    scheduled::ScheduledProducer,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const ROOT_URL: &str = "http://localhost:9324";
const DEFAULT_CFG: [(&str, &str); 3] = [
    ("AWS_DEFAULT_REGION", "localhost"),
    ("AWS_ACCESS_KEY_ID", "x"),
    ("AWS_SECRET_ACCESS_KEY", "x"),
];

/// Returns a [`QueueBuilder`] configured to connect to the SQS instance spawned by the file
/// `testing-docker-compose.yaml` in the root of the repository.
///
/// Additionally this will make a temporary queue on that instance for the duration of the test such
/// as to ensure there is no stealing.w
async fn make_test_queue() -> QueueBuilder<SqsBackend, Static> {
    for (var, val) in &DEFAULT_CFG {
        if std::env::var(var).is_err() {
            std::env::set_var(var, val);
        }
    }

    let config = aws_config::from_env().endpoint_url(ROOT_URL).load().await;
    let client = Client::new(&config);

    let queue_name: String = std::iter::repeat_with(fastrand::alphanumeric)
        .take(8)
        .collect();
    client
        .create_queue()
        .queue_name(&queue_name)
        .send()
        .await
        .unwrap();

    let config = SqsConfig {
        queue_dsn: format!("{ROOT_URL}/queue/{queue_name}"),
        override_endpoint: true,
    };

    SqsBackend::builder(config)
}

#[tokio::test]
async fn test_raw_send_recv() {
    let payload = "{\"test\": \"data\"}";
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_raw(&payload.to_owned()).await.unwrap();

    let d = c.receive().await.unwrap();
    assert_eq!(d.borrow_payload().unwrap(), payload.as_bytes());
}

#[tokio::test]
async fn test_bytes_send_recv() {
    let payload = b"hello";
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_bytes(payload).await.unwrap();

    let d = c.receive().await.unwrap();
    assert_eq!(d.borrow_payload().unwrap(), payload);
    d.ack().await.unwrap();
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct ExType {
    a: u8,
}

#[tokio::test]
async fn test_serde_send_recv() {
    let payload = ExType { a: 2 };
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_serde_json(&payload).await.unwrap();

    let d = c.receive().await.unwrap();
    assert_eq!(d.payload_serde_json::<ExType>().unwrap().unwrap(), payload);
    d.ack().await.unwrap();
}

#[tokio::test]
async fn test_custom_send_recv() {
    let payload = ExType { a: 3 };

    let encoder = |p: &ExType| Ok(format!("{}", p.a));
    let decoder = |p: &String| {
        Ok(ExType {
            a: p.parse().unwrap_or(0),
        })
    };

    let (p, mut c) = make_test_queue()
        .await
        .with_encoder(encoder)
        .with_decoder(decoder)
        .build_pair()
        .await
        .unwrap();

    p.send_custom(&payload).await.unwrap();

    let d = c.receive().await.unwrap();
    assert_eq!(d.payload_custom::<ExType>().unwrap().unwrap(), payload);

    // Because it doesn't use JSON, this should fail:
    d.payload_serde_json::<ExType>().unwrap_err();
    d.ack().await.unwrap();
}

/// Consumer will return immediately if there are fewer than max messages to start with.
#[tokio::test]
async fn test_send_recv_all_partial() {
    let payload = ExType { a: 2 };
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_serde_json(&payload).await.unwrap();
    let deadline = Duration::from_secs(1);

    let now = Instant::now();
    let mut xs = c.receive_all(2, deadline).await.unwrap();
    assert_eq!(xs.len(), 1);
    let d = xs.remove(0);
    assert_eq!(d.payload_serde_json::<ExType>().unwrap().unwrap(), payload);
    d.ack().await.unwrap();
    assert!(now.elapsed() <= deadline);
}

/// Consumer should yield items immediately if there's a full batch ready on the first poll.
#[tokio::test]
async fn test_send_recv_all_full() {
    let payload1 = ExType { a: 1 };
    let payload2 = ExType { a: 2 };
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_serde_json(&payload1).await.unwrap();
    p.send_serde_json(&payload2).await.unwrap();
    let deadline = Duration::from_secs(1);

    let now = Instant::now();
    let mut xs = c.receive_all(2, deadline).await.unwrap();
    assert_eq!(xs.len(), 2);
    let d1 = xs.remove(0);
    assert_eq!(
        d1.payload_serde_json::<ExType>().unwrap().unwrap(),
        payload1
    );
    d1.ack().await.unwrap();

    let d2 = xs.remove(0);
    assert_eq!(
        d2.payload_serde_json::<ExType>().unwrap().unwrap(),
        payload2
    );
    d2.ack().await.unwrap();
    // N.b. it's still possible this could turn up false if the test runs too slow.
    assert!(now.elapsed() < deadline);
}

/// Consumer will return the full batch immediately, but also return immediately if a partial batch is ready.
#[tokio::test]
async fn test_send_recv_all_full_then_partial() {
    let payload1 = ExType { a: 1 };
    let payload2 = ExType { a: 2 };
    let payload3 = ExType { a: 3 };
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    p.send_serde_json(&payload1).await.unwrap();
    p.send_serde_json(&payload2).await.unwrap();
    p.send_serde_json(&payload3).await.unwrap();

    let deadline = Duration::from_secs(1);
    let now1 = Instant::now();
    let mut xs = c.receive_all(2, deadline).await.unwrap();
    assert_eq!(xs.len(), 2);
    let d1 = xs.remove(0);
    assert_eq!(
        d1.payload_serde_json::<ExType>().unwrap().unwrap(),
        payload1
    );
    d1.ack().await.unwrap();

    let d2 = xs.remove(0);
    assert_eq!(
        d2.payload_serde_json::<ExType>().unwrap().unwrap(),
        payload2
    );
    d2.ack().await.unwrap();
    assert!(now1.elapsed() < deadline);

    // 2nd call
    let now2 = Instant::now();
    let mut ys = c.receive_all(2, deadline).await.unwrap();
    assert_eq!(ys.len(), 1);
    let d3 = ys.remove(0);
    assert_eq!(
        d3.payload_serde_json::<ExType>().unwrap().unwrap(),
        payload3
    );
    d3.ack().await.unwrap();
    assert!(now2.elapsed() < deadline);
}

/// Consumer will NOT wait indefinitely for at least one item.
#[tokio::test]
async fn test_send_recv_all_late_arriving_items() {
    let (_p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    let deadline = Duration::from_secs(1);
    let now = Instant::now();
    let xs = c.receive_all(2, deadline).await.unwrap();
    let elapsed = now.elapsed();

    assert_eq!(xs.len(), 0);
    // Elapsed should be around the deadline, ballpark
    assert!(elapsed >= deadline);
    assert!(elapsed <= deadline + Duration::from_millis(200));
}

#[tokio::test]
async fn test_scheduled() {
    let payload1 = ExType { a: 1 };
    let (p, mut c) = make_test_queue().await.build_pair().await.unwrap();

    let delay = Duration::from_secs(3);
    let now = Instant::now();
    p.send_serde_json_scheduled(&payload1, delay).await.unwrap();
    let delivery = c
        .receive_all(1, delay * 2)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(now.elapsed() >= delay);
    assert!(now.elapsed() < delay * 2);
    assert_eq!(Some(payload1), delivery.payload_serde_json().unwrap());
}
