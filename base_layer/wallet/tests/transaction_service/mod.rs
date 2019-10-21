// Copyright 2019. The Tari Project
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
// following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
// disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
// following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
// products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
// INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
// WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
// USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
use crate::support::{comms_and_services::setup_comms_services, utils::event_stream_count};
use futures::{
    channel::{
        mpsc,
        mpsc::{Receiver, Sender},
    },
    SinkExt,
    StreamExt,
};

use crate::support::{
    comms_and_services::create_dummy_message,
    utils::{make_input, TestParams},
};
use rand::OsRng;
use std::{sync::Arc, time::Duration};
use tari_broadcast_channel::bounded;
use tari_comms::{
    builder::CommsNode,
    message::Message,
    peer_manager::{NodeIdentity, PeerFeatures},
};
use tari_comms_dht::outbound::{DhtOutboundRequest, OutboundMessageRequester};
use tari_core::{
    tari_amount::*,
    transaction::OutputFeatures,
    transaction_protocol::{
        recipient::{RecipientSignedMessage, RecipientState},
        sender::TransactionSenderMessage,
    },
    types::{PrivateKey, PublicKey, COMMITMENT_FACTORY, PROVER},
    ReceiverTransactionProtocol,
};
use tari_crypto::keys::{PublicKey as PK, SecretKey as SK};
use tari_p2p::{
    comms_connector::pubsub_connector,
    domain_message::DomainMessage,
    services::comms_outbound::CommsOutboundServiceInitializer,
};
use tari_service_framework::{reply_channel, StackBuilder};
use tari_utilities::message_format::MessageFormat;
use tari_wallet::{
    output_manager_service::{
        handle::OutputManagerHandle,
        service::OutputManagerService,
        OutputManagerConfig,
        OutputManagerServiceInitializer,
    },
    transaction_service::{
        handle::{TransactionEvent, TransactionServiceHandle},
        service::TransactionService,
        TransactionServiceInitializer,
    },
};
use tokio::runtime::Runtime;

pub fn setup_transaction_service(
    runtime: &Runtime,
    master_key: PrivateKey,
    node_identity: NodeIdentity,
    peers: Vec<NodeIdentity>,
) -> (TransactionServiceHandle, OutputManagerHandle, CommsNode)
{
    let (publisher, subscription_factory) = pubsub_connector(runtime.executor(), 100);
    let subscription_factory = Arc::new(subscription_factory);
    let (comms, dht) = setup_comms_services(runtime.executor(), Arc::new(node_identity.clone()), peers, publisher);

    let fut = StackBuilder::new(runtime.executor(), comms.shutdown_signal())
        .add_initializer(CommsOutboundServiceInitializer::new(dht.outbound_requester()))
        .add_initializer(OutputManagerServiceInitializer::new(OutputManagerConfig {
            master_key,
            branch_seed: "".to_string(),
            primary_key_index: 0,
        }))
        .add_initializer(TransactionServiceInitializer::new(subscription_factory))
        .finish();

    let handles = runtime.block_on(fut).expect("Service initialization failed");

    let output_manager_handle = handles.get_handle::<OutputManagerHandle>().unwrap();
    let transaction_service_handle = handles.get_handle::<TransactionServiceHandle>().unwrap();

    (transaction_service_handle, output_manager_handle, comms)
}

/// This utility function creates a Transaction service without using the Service Framework Stack and exposes all the
/// streams for testing purposes.
pub fn setup_transaction_service_no_comms(
    runtime: &Runtime,
    master_key: PrivateKey,
) -> (
    TransactionServiceHandle,
    OutputManagerHandle,
    Receiver<DhtOutboundRequest>,
    Sender<DomainMessage<TransactionSenderMessage>>,
    Sender<DomainMessage<RecipientSignedMessage>>,
)
{
    let (oms_request_sender, oms_request_receiver) = reply_channel::unbounded();
    let output_manager_service = OutputManagerService::new(oms_request_receiver, master_key, "".to_string(), 0);
    let output_manager_service_handle = OutputManagerHandle::new(oms_request_sender);

    let (ts_request_sender, ts_request_receiver) = reply_channel::unbounded();
    let (event_publisher, event_subscriber) = bounded(100);
    let ts_handle = TransactionServiceHandle::new(ts_request_sender, event_subscriber);
    let (tx_sender, tx_receiver) = mpsc::channel(20);
    let (tx_ack_sender, tx_ack_receiver) = mpsc::channel(20);

    let (outbound_message_sender, outbound_message_receiver) = mpsc::channel(20);
    let outbound_message_requester = OutboundMessageRequester::new(outbound_message_sender);

    let ts_service = TransactionService::new(
        ts_request_receiver,
        tx_receiver,
        tx_ack_receiver,
        output_manager_service_handle.clone(),
        outbound_message_requester.clone(),
        event_publisher,
    );
    runtime.executor().spawn(async move {
        let _ = output_manager_service.start().await.unwrap();
    });
    runtime.executor().spawn(async move {
        let _ = ts_service.start().await.unwrap();
    });
    (
        ts_handle,
        output_manager_service_handle,
        outbound_message_receiver,
        tx_sender,
        tx_ack_sender,
    )
}

#[test]
fn manage_single_transaction() {
    let runtime = Runtime::new().unwrap();
    let mut rng = OsRng::new().unwrap();
    // Alice's parameters
    let alice_seed = PrivateKey::random(&mut rng);
    let alice_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31583".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();

    // Bob's parameters
    let bob_seed = PrivateKey::random(&mut rng);
    let bob_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31582".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();

    let (mut alice_ts, mut alice_oms, alice_comms) =
        setup_transaction_service(&runtime, alice_seed, alice_node_identity.clone(), vec![
            bob_node_identity.clone(),
        ]);
    let alice_event_stream = alice_ts.get_event_stream_fused();

    let value = MicroTari::from(1000);
    let (_utxo, uo1) = make_input(&mut rng, MicroTari(2500));

    assert!(runtime
        .block_on(alice_ts.send_transaction(
            bob_node_identity.identity.public_key.clone(),
            value,
            MicroTari::from(20),
        ))
        .is_err());

    runtime.block_on(alice_oms.add_output(uo1)).unwrap();

    runtime
        .block_on(alice_ts.send_transaction(
            bob_node_identity.identity.public_key.clone(),
            value,
            MicroTari::from(20),
        ))
        .unwrap();
    let alice_pending_outbound = runtime.block_on(alice_ts.get_pending_outbound_transactions()).unwrap();
    let alice_completed_tx = runtime.block_on(alice_ts.get_completed_transactions()).unwrap();
    assert_eq!(alice_pending_outbound.len(), 1);
    assert_eq!(alice_completed_tx.len(), 0);

    let (mut bob_ts, mut bob_oms, bob_comms) =
        setup_transaction_service(&runtime, bob_seed, bob_node_identity.clone(), vec![
            alice_node_identity.clone()
        ]);

    let mut result =
        runtime.block_on(async { event_stream_count(alice_event_stream, 1, Duration::from_secs(10)).await });
    assert_eq!(result.remove(&TransactionEvent::ReceivedTransactionReply), Some(1));

    let alice_pending_outbound = runtime.block_on(alice_ts.get_pending_outbound_transactions()).unwrap();
    let alice_completed_tx = runtime.block_on(alice_ts.get_completed_transactions()).unwrap();
    assert_eq!(alice_pending_outbound.len(), 0);
    assert_eq!(alice_completed_tx.len(), 1);

    let bob_pending_inbound_tx = runtime.block_on(bob_ts.get_pending_inbound_transactions()).unwrap();
    assert_eq!(bob_pending_inbound_tx.len(), 1);

    let mut alice_tx_id = 0;
    for (k, _v) in alice_completed_tx.iter() {
        alice_tx_id = k.clone();
    }
    for (k, v) in bob_pending_inbound_tx.iter() {
        assert_eq!(*k, alice_tx_id);
        if let RecipientState::Finalized(rsm) = &v.state {
            runtime
                .block_on(bob_oms.confirm_received_output(alice_tx_id, rsm.output.clone()))
                .unwrap();
            assert_eq!(runtime.block_on(bob_oms.get_balance()).unwrap(), value);
        } else {
            assert!(false);
        }
    }
    alice_comms.shutdown().unwrap();
    bob_comms.shutdown().unwrap();
}

#[test]
fn manage_multiple_transactions() {
    let runtime = Runtime::new().unwrap();
    let mut rng = OsRng::new().unwrap();
    // Alice's parameters
    let alice_seed = PrivateKey::random(&mut rng);
    let alice_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31584".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();

    // Bob's parameters
    let bob_seed = PrivateKey::random(&mut rng);
    let bob_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31585".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();

    // Carols's parameters
    let carol_seed = PrivateKey::random(&mut rng);
    let carol_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31586".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();

    let (mut alice_ts, mut alice_oms, alice_comms) =
        setup_transaction_service(&runtime, alice_seed, alice_node_identity.clone(), vec![
            bob_node_identity.clone(),
            carol_node_identity.clone(),
        ]);
    let alice_event_stream = alice_ts.get_event_stream_fused();

    // Add some funds to Alices wallet
    let (_utxo, uo1a) = make_input(&mut rng, MicroTari(5500));
    runtime.block_on(alice_oms.add_output(uo1a)).unwrap();
    let (_utxo, uo1b) = make_input(&mut rng, MicroTari(3000));
    runtime.block_on(alice_oms.add_output(uo1b)).unwrap();
    let (_utxo, uo1c) = make_input(&mut rng, MicroTari(3000));
    runtime.block_on(alice_oms.add_output(uo1c)).unwrap();

    // A series of interleaved transactions. First with Bob and Carol offline and then two with them online
    let value_a_to_b_1 = MicroTari::from(1000);
    let value_a_to_b_2 = MicroTari::from(800);
    let value_b_to_a_1 = MicroTari::from(1100);
    let value_a_to_c_1 = MicroTari::from(1400);
    runtime
        .block_on(alice_ts.send_transaction(
            bob_node_identity.identity.public_key.clone(),
            value_a_to_b_1,
            MicroTari::from(20),
        ))
        .unwrap();
    runtime
        .block_on(alice_ts.send_transaction(
            carol_node_identity.identity.public_key.clone(),
            value_a_to_c_1,
            MicroTari::from(20),
        ))
        .unwrap();
    let alice_pending_outbound = runtime.block_on(alice_ts.get_pending_outbound_transactions()).unwrap();
    let alice_completed_tx = runtime.block_on(alice_ts.get_completed_transactions()).unwrap();
    assert_eq!(alice_pending_outbound.len(), 2);
    assert_eq!(alice_completed_tx.len(), 0);

    // Spin up Bob and Carol
    let (mut bob_ts, mut bob_oms, bob_comms) =
        setup_transaction_service(&runtime, bob_seed, bob_node_identity.clone(), vec![
            alice_node_identity.clone()
        ]);
    let (mut carol_ts, mut carol_oms, carol_comms) =
        setup_transaction_service(&runtime, carol_seed, carol_node_identity.clone(), vec![
            alice_node_identity.clone(),
        ]);

    let (_utxo, uo2) = make_input(&mut rng, MicroTari(3500));
    runtime.block_on(bob_oms.add_output(uo2)).unwrap();
    let (_utxo, uo3) = make_input(&mut rng, MicroTari(4500));
    runtime.block_on(carol_oms.add_output(uo3)).unwrap();

    runtime
        .block_on(bob_ts.send_transaction(
            alice_node_identity.identity.public_key.clone(),
            value_b_to_a_1,
            MicroTari::from(20),
        ))
        .unwrap();
    runtime
        .block_on(alice_ts.send_transaction(
            bob_node_identity.identity.public_key.clone(),
            value_a_to_b_2,
            MicroTari::from(20),
        ))
        .unwrap();

    let mut result =
        runtime.block_on(async { event_stream_count(alice_event_stream, 4, Duration::from_secs(10)).await });
    assert_eq!(result.remove(&TransactionEvent::ReceivedTransactionReply), Some(3));

    let alice_pending_outbound = runtime.block_on(alice_ts.get_pending_outbound_transactions()).unwrap();
    let alice_completed_tx = runtime.block_on(alice_ts.get_completed_transactions()).unwrap();
    assert_eq!(alice_pending_outbound.len(), 0);
    assert_eq!(alice_completed_tx.len(), 3);
    let bob_pending_outbound = runtime.block_on(bob_ts.get_pending_outbound_transactions()).unwrap();
    let bob_completed_tx = runtime.block_on(bob_ts.get_completed_transactions()).unwrap();
    assert_eq!(bob_pending_outbound.len(), 0);
    assert_eq!(bob_completed_tx.len(), 1);
    let carol_pending_inbound = runtime.block_on(carol_ts.get_pending_inbound_transactions()).unwrap();
    assert_eq!(carol_pending_inbound.len(), 1);

    alice_comms.shutdown().unwrap();
    bob_comms.shutdown().unwrap();
    carol_comms.shutdown().unwrap();
}

#[test]
fn test_sending_repeated_tx_ids() {
    let runtime = Runtime::new().unwrap();
    let mut rng = OsRng::new().unwrap();

    let alice_seed = PrivateKey::random(&mut rng);
    let bob_seed = PrivateKey::random(&mut rng);

    let (alice_ts, _alice_output_manager, _alice_outbound_message_receiver, mut alice_tx_sender, _alice_tx_ack_sender) =
        setup_transaction_service_no_comms(&runtime, alice_seed);
    let (_bob_ts, mut bob_output_manager, _bob_outbound_message_receiver, _bob_tx_sender, _bob_tx_ack_sender) =
        setup_transaction_service_no_comms(&runtime, bob_seed);
    let alice_event_stream = alice_ts.get_event_stream_fused();

    let (_utxo, uo) = make_input(&mut rng, MicroTari(250000));

    runtime.block_on(bob_output_manager.add_output(uo)).unwrap();

    let mut stp = runtime
        .block_on(bob_output_manager.prepare_transaction_to_send(MicroTari::from(500), MicroTari::from(1000), None))
        .unwrap();
    let msg = stp.build_single_round_message().unwrap();
    let tx_message = create_dummy_message(TransactionSenderMessage::Single(Box::new(msg.clone())));

    runtime.block_on(alice_tx_sender.send(tx_message.clone())).unwrap();
    runtime.block_on(alice_tx_sender.send(tx_message.clone())).unwrap();

    let mut result =
        runtime.block_on(async { event_stream_count(alice_event_stream, 2, Duration::from_secs(10)).await });

    assert_eq!(result.remove(&TransactionEvent::ReceivedTransaction), Some(1));
    assert_eq!(
        result.remove(&TransactionEvent::Error(
            "Error handling Transaction Sender message".to_string()
        )),
        Some(1)
    );
}

#[test]
fn test_accepting_unknown_tx_id_and_malformed_reply() {
    let runtime = Runtime::new().unwrap();
    let mut rng = OsRng::new().unwrap();

    let alice_seed = PrivateKey::random(&mut rng);
    let bob_node_identity = NodeIdentity::random(
        &mut rng,
        "127.0.0.1:31585".parse().unwrap(),
        PeerFeatures::communication_node_default(),
    )
    .unwrap();
    let (
        mut alice_ts,
        mut alice_output_manager,
        mut alice_outbound_message_receiver,
        _alice_tx_sender,
        mut alice_tx_ack_sender,
    ) = setup_transaction_service_no_comms(&runtime, alice_seed);

    let alice_event_stream = alice_ts.get_event_stream_fused();

    let (_utxo, uo) = make_input(&mut rng, MicroTari(250000));

    runtime.block_on(alice_output_manager.add_output(uo)).unwrap();

    runtime
        .block_on(alice_ts.send_transaction(
            bob_node_identity.identity.public_key.clone(),
            MicroTari::from(500),
            MicroTari::from(1000),
        ))
        .unwrap();

    let mut serialized_msg = None;
    if let DhtOutboundRequest::SendMsg(req) = runtime.block_on(alice_outbound_message_receiver.next()).unwrap() {
        serialized_msg = Some(req.body);
    }

    let msg: Message = Message::from_binary(serialized_msg.unwrap().as_slice()).unwrap();
    let sender_message = msg.deserialize_message().unwrap();

    let params = TestParams::new(&mut rng);

    let rtp = ReceiverTransactionProtocol::new(
        sender_message,
        params.nonce,
        params.spend_key,
        OutputFeatures::default(),
        &PROVER,
        &COMMITMENT_FACTORY,
    );

    let mut tx_reply = rtp.get_signed_data().unwrap().clone();
    let mut wrong_tx_id = tx_reply.clone();
    wrong_tx_id.tx_id = 2;
    let (_p, pub_key) = PublicKey::random_keypair(&mut rng);
    tx_reply.public_spend_key = pub_key;
    runtime
        .block_on(alice_tx_ack_sender.send(create_dummy_message(wrong_tx_id)))
        .unwrap();

    runtime
        .block_on(alice_tx_ack_sender.send(create_dummy_message(tx_reply)))
        .unwrap();

    let mut result =
        runtime.block_on(async { event_stream_count(alice_event_stream, 2, Duration::from_secs(10)).await });
    assert_eq!(
        result.remove(&TransactionEvent::Error(
            "Error handling Transaction Recipient Reply message".to_string()
        )),
        Some(2)
    );
}
