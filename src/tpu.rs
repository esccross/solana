//! The `tpu` module implements the Transaction Processing Unit, a
//! 5-stage transaction processing pipeline in software.

use accounting_stage::AccountingStage;
use crdt::{Crdt, ReplicatedData};
use ecdsa;
use entry::Entry;
use ledger;
use packet;
use packet::SharedPackets;
use rand::{thread_rng, Rng};
use result::Result;
use serde_json;
use std::collections::VecDeque;
use std::io::Write;
use std::io::sink;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use std::time::Instant;
use streamer;
use thin_client_service::ThinClientService;
use timing;

pub struct Tpu {
    accounting_stage: AccountingStage,
    thin_client_service: ThinClientService,
}

type SharedTpu = Arc<Tpu>;

impl Tpu {
    /// Create a new Tpu that wraps the given Accountant.
    pub fn new(accounting_stage: AccountingStage) -> Self {
        let thin_client_service = ThinClientService::new(accounting_stage.accountant.clone());
        Tpu {
            accounting_stage,
            thin_client_service,
        }
    }

    fn write_entry<W: Write>(&self, writer: &Mutex<W>, entry: &Entry) {
        trace!("write_entry entry");
        self.accounting_stage
            .accountant
            .register_entry_id(&entry.id);
        writeln!(
            writer.lock().expect("'writer' lock in fn fn write_entry"),
            "{}",
            serde_json::to_string(&entry).expect("'entry' to_strong in fn write_entry")
        ).expect("writeln! in fn write_entry");
        self.thin_client_service
            .notify_entry_info_subscribers(&entry);
    }

    fn write_entries<W: Write>(&self, writer: &Mutex<W>) -> Result<Vec<Entry>> {
        //TODO implement a serialize for channel that does this without allocations
        let mut l = vec![];
        let entry = self.accounting_stage
            .output
            .lock()
            .expect("'ouput' lock in fn receive_all")
            .recv_timeout(Duration::new(1, 0))?;
        self.write_entry(writer, &entry);
        l.push(entry);
        while let Ok(entry) = self.accounting_stage
            .output
            .lock()
            .expect("'output' lock in fn write_entries")
            .try_recv()
        {
            self.write_entry(writer, &entry);
            l.push(entry);
        }
        Ok(l)
    }

    /// Process any Entry items that have been published by the Historian.
    /// continuosly broadcast blobs of entries out
    fn run_sync<W: Write>(
        &self,
        broadcast: &streamer::BlobSender,
        blob_recycler: &packet::BlobRecycler,
        writer: &Mutex<W>,
    ) -> Result<()> {
        let mut q = VecDeque::new();
        let list = self.write_entries(writer)?;
        trace!("New blobs? {}", list.len());
        ledger::process_entry_list_into_blobs(&list, blob_recycler, &mut q);
        if !q.is_empty() {
            broadcast.send(q)?;
        }
        Ok(())
    }

    pub fn sync_service<W: Write + Send + 'static>(
        obj: SharedTpu,
        exit: Arc<AtomicBool>,
        broadcast: streamer::BlobSender,
        blob_recycler: packet::BlobRecycler,
        writer: Mutex<W>,
    ) -> JoinHandle<()> {
        spawn(move || loop {
            let _ = obj.run_sync(&broadcast, &blob_recycler, &writer);
            if exit.load(Ordering::Relaxed) {
                info!("sync_service exiting");
                break;
            }
        })
    }

    /// Process any Entry items that have been published by the Historian.
    /// continuosly broadcast blobs of entries out
    fn run_sync_no_broadcast(&self) -> Result<()> {
        self.write_entries(&Arc::new(Mutex::new(sink())))?;
        Ok(())
    }

    pub fn sync_no_broadcast_service(obj: SharedTpu, exit: Arc<AtomicBool>) -> JoinHandle<()> {
        spawn(move || loop {
            let _ = obj.run_sync_no_broadcast();
            if exit.load(Ordering::Relaxed) {
                info!("sync_no_broadcast_service exiting");
                break;
            }
        })
    }

    fn verify_batch(
        batch: Vec<SharedPackets>,
        sendr: &Arc<Mutex<Sender<Vec<(SharedPackets, Vec<u8>)>>>>,
    ) -> Result<()> {
        let r = ecdsa::ed25519_verify(&batch);
        let res = batch.into_iter().zip(r).collect();
        sendr
            .lock()
            .expect("lock in fn verify_batch in tpu")
            .send(res)?;
        // TODO: fix error handling here?
        Ok(())
    }

    fn verifier(
        recvr: &Arc<Mutex<streamer::PacketReceiver>>,
        sendr: &Arc<Mutex<Sender<Vec<(SharedPackets, Vec<u8>)>>>>,
    ) -> Result<()> {
        let (batch, len) =
            streamer::recv_batch(&recvr.lock().expect("'recvr' lock in fn verifier"))?;

        let now = Instant::now();
        let batch_len = batch.len();
        let rand_id = thread_rng().gen_range(0, 100);
        info!(
            "@{:?} verifier: verifying: {} id: {}",
            timing::timestamp(),
            batch.len(),
            rand_id
        );

        Self::verify_batch(batch, sendr).expect("verify_batch in fn verifier");

        let total_time_ms = timing::duration_as_ms(&now.elapsed());
        let total_time_s = timing::duration_as_s(&now.elapsed());
        info!(
            "@{:?} verifier: done. batches: {} total verify time: {:?} id: {} verified: {} v/s {}",
            timing::timestamp(),
            batch_len,
            total_time_ms,
            rand_id,
            len,
            (len as f32 / total_time_s)
        );
        Ok(())
    }

    /// Process verified blobs, already in order
    /// Respond with a signed hash of the state
    fn replicate_state(
        obj: &Tpu,
        verified_receiver: &streamer::BlobReceiver,
        blob_recycler: &packet::BlobRecycler,
    ) -> Result<()> {
        let timer = Duration::new(1, 0);
        let blobs = verified_receiver.recv_timeout(timer)?;
        trace!("replicating blobs {}", blobs.len());
        let entries = ledger::reconstruct_entries_from_blobs(&blobs);
        obj.accounting_stage
            .accountant
            .process_verified_entries(entries)?;
        for blob in blobs {
            blob_recycler.recycle(blob);
        }
        Ok(())
    }

    /// Create a UDP microservice that forwards messages the given Tpu.
    /// This service is the network leader
    /// Set `exit` to shutdown its threads.
    pub fn serve<W: Write + Send + 'static>(
        obj: &SharedTpu,
        me: ReplicatedData,
        serve: UdpSocket,
        _events_socket: UdpSocket,
        gossip: UdpSocket,
        exit: Arc<AtomicBool>,
        writer: W,
    ) -> Result<Vec<JoinHandle<()>>> {
        let crdt = Arc::new(RwLock::new(Crdt::new(me)));
        let t_gossip = Crdt::gossip(crdt.clone(), exit.clone());
        let t_listen = Crdt::listen(crdt.clone(), gossip, exit.clone());

        // make sure we are on the same interface
        let mut local = serve.local_addr()?;
        local.set_port(0);
        let respond_socket = UdpSocket::bind(local.clone())?;

        let packet_recycler = packet::PacketRecycler::default();
        let blob_recycler = packet::BlobRecycler::default();
        let (packet_sender, packet_receiver) = channel();
        let t_receiver =
            streamer::receiver(serve, exit.clone(), packet_recycler.clone(), packet_sender)?;
        let (responder_sender, responder_receiver) = channel();
        let t_responder = streamer::responder(
            respond_socket,
            exit.clone(),
            blob_recycler.clone(),
            responder_receiver,
        );
        let (verified_sender, verified_receiver) = channel();

        let mut verify_threads = Vec::new();
        let shared_verified_sender = Arc::new(Mutex::new(verified_sender));
        let shared_packet_receiver = Arc::new(Mutex::new(packet_receiver));
        for _ in 0..4 {
            let exit_ = exit.clone();
            let recv = shared_packet_receiver.clone();
            let sender = shared_verified_sender.clone();
            let thread = spawn(move || loop {
                let e = Self::verifier(&recv, &sender);
                if e.is_err() && exit_.load(Ordering::Relaxed) {
                    break;
                }
            });
            verify_threads.push(thread);
        }

        let (broadcast_sender, broadcast_receiver) = channel();

        let broadcast_socket = UdpSocket::bind(local)?;
        let t_broadcast = streamer::broadcaster(
            broadcast_socket,
            exit.clone(),
            crdt.clone(),
            blob_recycler.clone(),
            broadcast_receiver,
        );

        let t_sync = Self::sync_service(
            obj.clone(),
            exit.clone(),
            broadcast_sender,
            blob_recycler.clone(),
            Mutex::new(writer),
        );

        let tpu = obj.clone();
        let t_server = spawn(move || loop {
            let e = tpu.thin_client_service.process_request_packets(
                &tpu.accounting_stage,
                &verified_receiver,
                &responder_sender,
                &packet_recycler,
                &blob_recycler,
            );
            if e.is_err() {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let mut threads = vec![
            t_receiver,
            t_responder,
            t_server,
            t_sync,
            t_gossip,
            t_listen,
            t_broadcast,
        ];
        threads.extend(verify_threads.into_iter());
        Ok(threads)
    }

    /// This service receives messages from a leader in the network and processes the transactions
    /// on the accountant state.
    /// # Arguments
    /// * `obj` - The accountant state.
    /// * `me` - my configuration
    /// * `leader` - leader configuration
    /// * `exit` - The exit signal.
    /// # Remarks
    /// The pipeline is constructed as follows:
    /// 1. receive blobs from the network, these are out of order
    /// 2. verify blobs, PoH, signatures (TODO)
    /// 3. reconstruct contiguous window
    ///     a. order the blobs
    ///     b. use erasure coding to reconstruct missing blobs
    ///     c. ask the network for missing blobs, if erasure coding is insufficient
    ///     d. make sure that the blobs PoH sequences connect (TODO)
    /// 4. process the transaction state machine
    /// 5. respond with the hash of the state back to the leader
    pub fn replicate(
        obj: &SharedTpu,
        me: ReplicatedData,
        gossip: UdpSocket,
        serve: UdpSocket,
        replicate: UdpSocket,
        leader: ReplicatedData,
        exit: Arc<AtomicBool>,
    ) -> Result<Vec<JoinHandle<()>>> {
        //replicate pipeline
        let crdt = Arc::new(RwLock::new(Crdt::new(me)));
        crdt.write()
            .expect("'crdt' write lock in pub fn replicate")
            .set_leader(leader.id);
        crdt.write()
            .expect("'crdt' write lock before insert() in pub fn replicate")
            .insert(leader);
        let t_gossip = Crdt::gossip(crdt.clone(), exit.clone());
        let t_listen = Crdt::listen(crdt.clone(), gossip, exit.clone());

        // make sure we are on the same interface
        let mut local = replicate.local_addr()?;
        local.set_port(0);
        let write = UdpSocket::bind(local)?;

        let blob_recycler = packet::BlobRecycler::default();
        let (blob_sender, blob_receiver) = channel();
        let t_blob_receiver = streamer::blob_receiver(
            exit.clone(),
            blob_recycler.clone(),
            replicate,
            blob_sender.clone(),
        )?;
        let (window_sender, window_receiver) = channel();
        let (retransmit_sender, retransmit_receiver) = channel();

        let t_retransmit = streamer::retransmitter(
            write,
            exit.clone(),
            crdt.clone(),
            blob_recycler.clone(),
            retransmit_receiver,
        );

        //TODO
        //the packets coming out of blob_receiver need to be sent to the GPU and verified
        //then sent to the window, which does the erasure coding reconstruction
        let t_window = streamer::window(
            exit.clone(),
            crdt.clone(),
            blob_recycler.clone(),
            blob_receiver,
            window_sender,
            retransmit_sender,
        );

        let tpu = obj.clone();
        let s_exit = exit.clone();
        let t_replicator = spawn(move || loop {
            let e = Self::replicate_state(&tpu, &window_receiver, &blob_recycler);
            if e.is_err() && s_exit.load(Ordering::Relaxed) {
                break;
            }
        });

        //serve pipeline
        // make sure we are on the same interface
        let mut local = serve.local_addr()?;
        local.set_port(0);
        let respond_socket = UdpSocket::bind(local.clone())?;

        let packet_recycler = packet::PacketRecycler::default();
        let blob_recycler = packet::BlobRecycler::default();
        let (packet_sender, packet_receiver) = channel();
        let t_packet_receiver =
            streamer::receiver(serve, exit.clone(), packet_recycler.clone(), packet_sender)?;
        let (responder_sender, responder_receiver) = channel();
        let t_responder = streamer::responder(
            respond_socket,
            exit.clone(),
            blob_recycler.clone(),
            responder_receiver,
        );
        let (verified_sender, verified_receiver) = channel();

        let mut verify_threads = Vec::new();
        let shared_verified_sender = Arc::new(Mutex::new(verified_sender));
        let shared_packet_receiver = Arc::new(Mutex::new(packet_receiver));
        for _ in 0..4 {
            let exit_ = exit.clone();
            let recv = shared_packet_receiver.clone();
            let sender = shared_verified_sender.clone();
            let thread = spawn(move || loop {
                let e = Self::verifier(&recv, &sender);
                if e.is_err() && exit_.load(Ordering::Relaxed) {
                    break;
                }
            });
            verify_threads.push(thread);
        }
        let t_sync = Self::sync_no_broadcast_service(obj.clone(), exit.clone());

        let tpu = obj.clone();
        let s_exit = exit.clone();
        let t_server = spawn(move || loop {
            let e = tpu.thin_client_service.process_request_packets(
                &tpu.accounting_stage,
                &verified_receiver,
                &responder_sender,
                &packet_recycler,
                &blob_recycler,
            );
            if e.is_err() {
                if s_exit.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let mut threads = vec![
            //replicate threads
            t_blob_receiver,
            t_retransmit,
            t_window,
            t_replicator,
            t_gossip,
            t_listen,
            //serve threads
            t_packet_receiver,
            t_responder,
            t_server,
            t_sync,
        ];
        threads.extend(verify_threads.into_iter());
        Ok(threads)
    }
}

#[cfg(test)]
pub fn test_node() -> (ReplicatedData, UdpSocket, UdpSocket, UdpSocket, UdpSocket) {
    use signature::{KeyPair, KeyPairUtil};

    let events_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let gossip = UdpSocket::bind("127.0.0.1:0").unwrap();
    let replicate = UdpSocket::bind("127.0.0.1:0").unwrap();
    let serve = UdpSocket::bind("127.0.0.1:0").unwrap();
    let pubkey = KeyPair::new().pubkey();
    let d = ReplicatedData::new(
        pubkey,
        gossip.local_addr().unwrap(),
        replicate.local_addr().unwrap(),
        serve.local_addr().unwrap(),
    );
    (d, gossip, replicate, serve, events_socket)
}

#[cfg(test)]
mod tests {
    use accountant::Accountant;
    use accounting_stage::AccountingStage;
    use bincode::serialize;
    use chrono::prelude::*;
    use crdt::Crdt;
    use entry;
    use event::Event;
    use hash::{hash, Hash};
    use logger;
    use mint::Mint;
    use packet::BlobRecycler;
    use signature::{KeyPair, KeyPairUtil};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use streamer;
    use tpu::{test_node, Tpu};
    use transaction::Transaction;

    /// Test that mesasge sent from leader to target1 and repliated to target2
    #[test]
    #[ignore]
    fn test_replicate() {
        logger::setup();
        let (leader_data, leader_gossip, _, leader_serve, _) = test_node();
        let (target1_data, target1_gossip, target1_replicate, target1_serve, _) = test_node();
        let (target2_data, target2_gossip, target2_replicate, _, _) = test_node();
        let exit = Arc::new(AtomicBool::new(false));

        //start crdt_leader
        let mut crdt_l = Crdt::new(leader_data.clone());
        crdt_l.set_leader(leader_data.id);

        let cref_l = Arc::new(RwLock::new(crdt_l));
        let t_l_gossip = Crdt::gossip(cref_l.clone(), exit.clone());
        let t_l_listen = Crdt::listen(cref_l, leader_gossip, exit.clone());

        //start crdt2
        let mut crdt2 = Crdt::new(target2_data.clone());
        crdt2.insert(leader_data.clone());
        crdt2.set_leader(leader_data.id);
        let leader_id = leader_data.id;
        let cref2 = Arc::new(RwLock::new(crdt2));
        let t2_gossip = Crdt::gossip(cref2.clone(), exit.clone());
        let t2_listen = Crdt::listen(cref2, target2_gossip, exit.clone());

        // setup some blob services to send blobs into the socket
        // to simulate the source peer and get blobs out of the socket to
        // simulate target peer
        let recv_recycler = BlobRecycler::default();
        let resp_recycler = BlobRecycler::default();
        let (s_reader, r_reader) = channel();
        let t_receiver = streamer::blob_receiver(
            exit.clone(),
            recv_recycler.clone(),
            target2_replicate,
            s_reader,
        ).unwrap();

        // simulate leader sending messages
        let (s_responder, r_responder) = channel();
        let t_responder = streamer::responder(
            leader_serve,
            exit.clone(),
            resp_recycler.clone(),
            r_responder,
        );

        let starting_balance = 10_000;
        let alice = Mint::new(starting_balance);
        let accountant = Accountant::new(&alice);
        let accounting_stage = AccountingStage::new(accountant, &alice.last_id(), Some(30));
        let tpu = Arc::new(Tpu::new(accounting_stage));
        let replicate_addr = target1_data.replicate_addr;
        let threads = Tpu::replicate(
            &tpu,
            target1_data,
            target1_gossip,
            target1_serve,
            target1_replicate,
            leader_data,
            exit.clone(),
        ).unwrap();

        let mut alice_ref_balance = starting_balance;
        let mut msgs = VecDeque::new();
        let mut cur_hash = Hash::default();
        let num_blobs = 10;
        let transfer_amount = 501;
        let bob_keypair = KeyPair::new();
        for i in 0..num_blobs {
            let b = resp_recycler.allocate();
            let b_ = b.clone();
            let mut w = b.write().unwrap();
            w.set_index(i).unwrap();
            w.set_id(leader_id).unwrap();

            let accountant = &tpu.accounting_stage.accountant;

            let tr0 = Event::new_timestamp(&bob_keypair, Utc::now());
            let entry0 = entry::create_entry(&cur_hash, i, vec![tr0]);
            accountant.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash);

            let tr1 = Transaction::new(
                &alice.keypair(),
                bob_keypair.pubkey(),
                transfer_amount,
                cur_hash,
            );
            accountant.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash);
            let entry1 =
                entry::create_entry(&cur_hash, i + num_blobs, vec![Event::Transaction(tr1)]);
            accountant.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash);

            alice_ref_balance -= transfer_amount;

            let serialized_entry = serialize(&vec![entry0, entry1]).unwrap();

            w.data_mut()[..serialized_entry.len()].copy_from_slice(&serialized_entry);
            w.set_size(serialized_entry.len());
            w.meta.set_addr(&replicate_addr);
            drop(w);
            msgs.push_back(b_);
        }

        // send the blobs into the socket
        s_responder.send(msgs).expect("send");

        // receive retransmitted messages
        let timer = Duration::new(1, 0);
        let mut msgs: Vec<_> = Vec::new();
        while let Ok(msg) = r_reader.recv_timeout(timer) {
            trace!("msg: {:?}", msg);
            msgs.push(msg);
        }

        let accountant = &tpu.accounting_stage.accountant;
        let alice_balance = accountant.get_balance(&alice.keypair().pubkey()).unwrap();
        assert_eq!(alice_balance, alice_ref_balance);

        let bob_balance = accountant.get_balance(&bob_keypair.pubkey()).unwrap();
        assert_eq!(bob_balance, starting_balance - alice_ref_balance);

        exit.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().expect("join");
        }
        t2_gossip.join().expect("join");
        t2_listen.join().expect("join");
        t_receiver.join().expect("join");
        t_responder.join().expect("join");
        t_l_gossip.join().expect("join");
        t_l_listen.join().expect("join");
    }

}
