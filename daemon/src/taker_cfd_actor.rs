use crate::db::{
    insert_cfd, insert_new_cfd_state_by_order_id, insert_order, load_all_cfds,
    load_cfd_by_order_id, load_order_by_id,
};
use crate::model::cfd::{Cfd, CfdState, CfdStateCommon, FinalizedCfd, Order, OrderId};
use crate::model::{Usd, WalletInfo};
use crate::wallet::Wallet;
use crate::wire::SetupMsg;
use crate::{setup_contract_actor, wire};
use bdk::bitcoin::secp256k1::schnorrsig;
use core::panic;
use futures::Future;
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    SyncWallet,
    TakeOrder { order_id: OrderId, quantity: Usd },
    NewOrder(Option<Order>),
    OrderAccepted(OrderId),
    IncProtocolMsg(SetupMsg),
    CfdSetupCompleted(FinalizedCfd),
}

pub fn new(
    db: sqlx::SqlitePool,
    wallet: Wallet,
    oracle_pk: schnorrsig::PublicKey,
    cfd_feed_actor_inbox: watch::Sender<Vec<Cfd>>,
    order_feed_actor_inbox: watch::Sender<Option<Order>>,
    out_msg_maker_inbox: mpsc::UnboundedSender<wire::TakerToMaker>,
    wallet_feed_sender: watch::Sender<WalletInfo>,
) -> (impl Future<Output = ()>, mpsc::UnboundedSender<Command>) {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut current_contract_setup = None;

    let actor = {
        let sender = sender.clone();

        async move {
            // populate the CFD feed with existing CFDs
            let mut conn = db.acquire().await.unwrap();
            cfd_feed_actor_inbox
                .send(load_all_cfds(&mut conn).await.unwrap())
                .unwrap();

            while let Some(message) = receiver.recv().await {
                match message {
                    Command::SyncWallet => {
                        let wallet_info = wallet.sync().unwrap();
                        wallet_feed_sender.send(wallet_info).unwrap();
                    }
                    Command::TakeOrder { order_id, quantity } => {
                        let mut conn = db.acquire().await.unwrap();

                        let current_order = load_order_by_id(order_id, &mut conn).await.unwrap();

                        println!("Accepting current order: {:?}", &current_order);

                        let cfd = Cfd::new(
                            current_order.clone(),
                            quantity,
                            CfdState::PendingTakeRequest {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                        );

                        insert_cfd(cfd, &mut conn).await.unwrap();

                        cfd_feed_actor_inbox
                            .send(load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();
                        out_msg_maker_inbox
                            .send(wire::TakerToMaker::TakeOrder { order_id, quantity })
                            .unwrap();
                    }
                    Command::NewOrder(Some(order)) => {
                        let mut conn = db.acquire().await.unwrap();
                        insert_order(&order, &mut conn).await.unwrap();
                        order_feed_actor_inbox.send(Some(order)).unwrap();
                    }

                    Command::NewOrder(None) => {
                        order_feed_actor_inbox.send(None).unwrap();
                    }
                    Command::OrderAccepted(order_id) => {
                        let mut conn = db.acquire().await.unwrap();
                        insert_new_cfd_state_by_order_id(
                            order_id,
                            CfdState::ContractSetup {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                            &mut conn,
                        )
                        .await
                        .unwrap();

                        cfd_feed_actor_inbox
                            .send(load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();

                        let (sk, pk) = crate::keypair::new(&mut rand::thread_rng());

                        let cfd = load_cfd_by_order_id(order_id, &mut conn).await.unwrap();
                        let margin = cfd.margin().unwrap();

                        let taker_params = wallet.build_party_params(margin, pk).unwrap();

                        let (actor, inbox) = setup_contract_actor::new(
                            {
                                let inbox = out_msg_maker_inbox.clone();
                                move |msg| inbox.send(wire::TakerToMaker::Protocol(msg)).unwrap()
                            },
                            setup_contract_actor::OwnParams::Taker(taker_params),
                            sk,
                            oracle_pk,
                            cfd,
                        );

                        tokio::spawn({
                            let sender = sender.clone();

                            async move {
                                sender
                                    .send(Command::CfdSetupCompleted(actor.await))
                                    .unwrap()
                            }
                        });
                        current_contract_setup = Some(inbox);
                    }
                    Command::IncProtocolMsg(msg) => {
                        let inbox = match &current_contract_setup {
                            None => panic!("whoops"),
                            Some(inbox) => inbox,
                        };

                        inbox.send(msg).unwrap();
                    }
                    Command::CfdSetupCompleted(_finalized_cfd) => {
                        todo!("but what?")

                        // Assumption: The maker publishes the CFD on chain
                    }
                }
            }
        }
    };

    (actor, sender)
}