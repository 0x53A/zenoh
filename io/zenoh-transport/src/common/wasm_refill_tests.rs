//! Browser-only regression hook enabled by the existing internal `test` feature.
use super::*;
use super::super::{batch::BatchConfig, priority::TransportPriorityTx};
use std::{sync::Arc, time::Duration};
use zenoh_config::{QueueAllocConf, QueueAllocMode};
use zenoh_protocol::{core::{Bits, CongestionControl, Priority}, network::{ext, NetworkMessage, NetworkMessageExt, NetworkBody, Push}, transport::{BatchSize, TransportSn}};
use zenoh_runtime::{ZRuntime, wasm_yield::{sleep_ms, Instant}};
use zenoh_result::ZResult;

fn pipeline() -> (TransmissionPipelineProducer, TransmissionPipelineConsumer) {
    let config = TransmissionPipelineConf {
        batch: BatchConfig { mtu: 8192, is_streamed: false, #[cfg(feature="transport_compression")] is_compression:false },
        queue_size: [1; Priority::NUM], batching_enabled:false,
        wait_before_drop: Duration::from_millis(120), max_wait_before_drop_fragments: Duration::from_millis(120),
        wait_before_close: Duration::from_millis(120), batching_time_limit: Duration::ZERO,
        queue_alloc: QueueAllocConf {mode:QueueAllocMode::Init},
    };
    let priority=TransportPriorityTx::make(Bits::from(TransportSn::MAX)).unwrap();
    TransmissionPipeline::make(config,&[priority],false)
}
fn message() -> NetworkMessage {
    NetworkMessage::from(NetworkBody::Push(Push {wire_expr:"refill-test".into(),ext_qos:ext::QoSType::new(Priority::Data,CongestionControl::Block,false),..Push::from(zenoh_protocol::zenoh::PushBody::Put(zenoh_protocol::zenoh::Put { payload: vec![0u8;7000].into(), ..Default::default() }))}))
}

/// Exercises the real bounded pipeline from browser and dedicated workers.
pub async fn test_wasm_refill() -> ZResult<()> {
    // The main/JS thread must never park when the one-batch pool is exhausted.
    let (producer, _consumer)=pipeline();
    assert!(producer.push_network_message(message().as_ref())?);
    let start=Instant::now();
    assert!(!producer.push_network_message(message().as_ref())?);
    assert!(start.elapsed()<Duration::from_millis(100));

    // Two producers share Application; the consumer runs independently on TX.
    // The delayed first refill forces actual pool exhaustion, not idle traffic.
    let (producer,mut consumer)=pipeline();
    let producer=Arc::new(producer);
    let (done, results)=flume::unbounded();
    let consumer_task=ZRuntime::TX.spawn(async move {
        sleep_ms(60).await;
        for _ in 0..40 {
            let (batch,priority)=consumer.pull().await.unwrap();
            sleep_ms(2).await;
            consumer.refill(batch,priority);
        }
    });
    for _ in 0..2 {
        let producer=producer.clone();let done=done.clone();
        ZRuntime::Application.spawn(async move {
            let start=Instant::now();
            let ok=(0..20).all(|_| producer.push_network_message(message().as_ref()).unwrap());
            done.send((ok,start.elapsed())).unwrap();
        });
    }
    let mut observed_wait=false;
    for _ in 0..2 {
        let (ok,elapsed)=zenoh_runtime::recv_async_anywhere(&results).await?;
        assert!(ok,"transient full batch pool must recover");
        observed_wait|=elapsed>=Duration::from_millis(50);
    }
    consumer_task.await.unwrap();
    assert!(observed_wait,"the test must exercise refill waiting");

    // A genuinely stalled consumer must retain the configured finite deadline.
    let (done,result)=flume::bounded(1);
    ZRuntime::Application.spawn(async move {
        let (producer,_consumer)=pipeline();
        assert!(producer.push_network_message(message().as_ref()).unwrap());
        let start=Instant::now();
        let pushed=producer.push_network_message(message().as_ref()).unwrap();
        done.send((pushed,start.elapsed())).unwrap();
    });
    let (pushed,elapsed)=zenoh_runtime::recv_async_anywhere(&result).await?;
    assert!(!pushed && elapsed>=Duration::from_millis(100) && elapsed<Duration::from_secs(1),"full queue must time out at its real deadline: {elapsed:?}");
    // Notifications without a returned batch cannot restart the timeout.
    let (producer,consumer)=pipeline();
    let (done,result)=flume::bounded(1);
    let notifier_task=ZRuntime::TX.spawn(async move {
        for _ in 0..10 {
            sleep_ms(20).await;
            let _=consumer.stage_out[0].s_ref.n_ref_w.notify();
        }
    });
    ZRuntime::Application.spawn(async move {
        assert!(producer.push_network_message(message().as_ref()).unwrap());
        let start=Instant::now();
        let pushed=producer.push_network_message(message().as_ref()).unwrap();
        done.send((pushed,start.elapsed())).unwrap();
    });
    let (pushed,elapsed)=zenoh_runtime::recv_async_anywhere(&result).await?;
    assert!(!pushed && elapsed>=Duration::from_millis(100) && elapsed<Duration::from_millis(190),"spurious wakes extended deadline: {elapsed:?}");
    notifier_task.await.unwrap();

    // Consumer closure wakes the producer before its deadline.
    let (producer,consumer)=pipeline();
    let (done,result)=flume::bounded(1);
    ZRuntime::TX.spawn(async move {sleep_ms(30).await;drop(consumer);});
    ZRuntime::Application.spawn(async move {
        assert!(producer.push_network_message(message().as_ref()).unwrap());
        let start=Instant::now();
        let result=producer.push_network_message(message().as_ref());
        done.send((result.is_err(),start.elapsed())).unwrap();
    });
    let (closed,elapsed)=zenoh_runtime::recv_async_anywhere(&result).await?;
    assert!(closed && elapsed<Duration::from_millis(110),"consumer closure must wake blocked producer: {elapsed:?}");

    // A TX caller cannot wait for a consumer running on that same worker.
    let (done,result)=flume::bounded(1);
    ZRuntime::TX.spawn(async move {
        let (producer,_consumer)=pipeline();
        assert!(producer.push_network_message(message().as_ref()).unwrap());
        let start=Instant::now();
        assert!(!producer.push_network_message(message().as_ref()).unwrap());
        done.send(start.elapsed()).unwrap();
    });
    assert!(zenoh_runtime::recv_async_anywhere(&result).await?<Duration::from_millis(100));
    // A parked producer holds serialization locks while waiting for TX refill.
    // Main and TX publishers must fail promptly rather than parking on those locks.
    for on_tx in [false, true] {
        let (producer, _consumer)=pipeline();
        let producer=Arc::new(producer);
        let holder=producer.clone();
        let (locked, lock_ready)=flume::bounded(1);
        let parked=ZRuntime::Application.spawn(async move {
            let _guard=holder.stage_in[0].lock().unwrap();
            locked.send(()).unwrap();
            let mutex=std::sync::Mutex::new(());
            let _=std::sync::Condvar::new().wait_timeout(mutex.lock().unwrap(),Duration::from_millis(150)).unwrap();
        });
        zenoh_runtime::recv_async_anywhere(&lock_ready).await?;
        let attempt=move || {
            let start=Instant::now();
            let sent=producer.push_network_message(message().as_ref()).unwrap();
            !sent && start.elapsed()<Duration::from_millis(100)
        };
        let passed=if on_tx {ZRuntime::TX.spawn(async move {attempt()}).await.unwrap()} else {attempt()};
        parked.await.unwrap();
        assert!(passed,"contended JS/TX publisher must return an observable no-send result");
    }
    // The current-batch lock may be owned by the consumer independently of
    // the producer queue lock. This failure must not allocate/consume a batch.
    let (producer,_consumer)=pipeline();
    let current=producer.stage_in[0].lock().unwrap().mutex.current.clone();
    let guard=current.lock().unwrap();
    assert!(!producer.push_network_message(message().as_ref())?);
    drop(guard);
    assert!(producer.push_network_message(message().as_ref())?,"current-lock failure changed the batch pool");
    // Failure at the channel-sequence lock must restore the already pulled batch.
    let (producer,_consumer)=pipeline();
    let priority=producer.stage_in[0].lock().unwrap().mutex.priority.clone();
    let reliable=message().is_reliable();
    let guard=if reliable {priority.reliable.lock().unwrap()} else {priority.best_effort.lock().unwrap()};
    assert!(!producer.push_network_message(message().as_ref())?);
    drop(guard);
    assert!(producer.push_network_message(message().as_ref())?,"failed lock acquisition lost its batch");
    Ok(())
}
