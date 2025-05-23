mod empty;
mod merkle;
mod sync;
mod transaction;
mod types;

pub use types::*;
use web3::{contract::tokens::Tokenizable, ethabi::Token, signing::keccak256};

use std::{
    env::VarError,
    io::Read,
    path::PathBuf,
    process::Command,
    str::FromStr,
    sync::{mpsc, Arc, Mutex},
};

use contracts::{
    util::{convert_element_to_h256, convert_h160_to_element},
    Address, RollupContract, SecretKey, USDCContract,
};
use futures::Future;
// use ethereum_types::H160;
// use node::util::public_to_address;
use once_cell::sync::Lazy;
use primitives::hash::CryptoHash;
use reqwest::Url;
use serde_json::json;
use sha3::Digest;
use testutil::{eth::EthNode, PortPool};
use tokio::runtime::RuntimeFlavor;
use wire_message::WireMessage;
use zk_circuits::{
    constants::MERKLE_TREE_DEPTH,
    data::{Burn, BurnTo, InputNote, Mint, Note, ParameterSet, SnarkWitness},
    CircuitKind,
};
use zk_primitives::Element;

type Error = serde_json::Value;

fn find_binary() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    #[cfg(debug_assertions)]
    let target = "debug";
    #[cfg(not(debug_assertions))]
    let target = "release";
    path.push("../../target/");
    path.push(target);
    path.push("node");
    path
}

static PORT_POOL: Lazy<Mutex<PortPool>> =
    once_cell::sync::Lazy::new(|| Mutex::new(PortPool::new(12001..12001 + 1000)));

#[derive(Debug)]
struct ServerConfig {
    keep_port_after_drop: bool,
    safe_eth_height_offset: u64,
    rollup_contract: Address,
    secret_key: [u8; 32],
    mock_prover: bool,
}

impl ServerConfig {
    fn single_node(keep_port_after_drop: bool) -> Self {
        Self {
            keep_port_after_drop,
            safe_eth_height_offset: 0,
            rollup_contract: Address::from_slice(
                &hex::decode("2279b7a0a67db372996a5fab50d91eaa73d2ebe6").unwrap(),
            ),
            secret_key: hex::decode(
                "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            )
            .unwrap()
            .try_into()
            .unwrap(),
            mock_prover: false,
        }
    }

    fn mock_prover(keep_port_after_drop: bool) -> Self {
        Self {
            mock_prover: true,
            ..Self::single_node(keep_port_after_drop)
        }
    }

    // TODO: once we bring this back, we need to configure EthNode to spawn with multiple validators
    // fn four_nodes(keep_port_after_drop: bool) -> [Self; 4] {
    //     let config = |secret_key| Self {
    //         keep_port_after_drop,
    //         rollup_contract: Address::from_slice(
    //             &hex::decode(
    //                 "2279b7a0a67db372996a5fab50d91eaa73d2ebe6",
    //             )
    //             .unwrap(),
    //         ),
    //         secret_key: hex::decode(secret_key).unwrap().try_into().unwrap(),
    //         mock_prover: false,
    //     };

    //     // First 4 default hardhat accounts
    //     [
    //         config("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
    //         config("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"),
    //         config("5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"),
    //         config("7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"),
    //     ]
    // }
}

#[derive(Debug)]
struct Server {
    process: Option<std::process::Child>,
    // Keep the root dir alive so that server can use it
    root_dir: tempdir::TempDir,
    api_port: u16,
    p2p_port: u16,
    secret_key: [u8; 32],
    rollup_contract_addr: Address,
    // address: H160,
    peers: Vec<Peer>,
    keep_port_after_drop: bool,
    safe_eth_height_offset: u64,
    prover: bool,
    client: reqwest::Client,
    eth_node: Arc<EthNode>,
    stdout: mpsc::Receiver<String>,
    stdout_sender: Option<mpsc::Sender<String>>,
    stderr: mpsc::Receiver<String>,
    stderr_sender: Option<mpsc::Sender<String>>,
    output_readers: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct Peer {
    p2p_port: u16,
    // _address: H160,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        if !self.keep_port_after_drop {
            PORT_POOL.lock().unwrap().release(self.api_port);
            PORT_POOL.lock().unwrap().release(self.p2p_port);
        }
    }
}

impl Server {
    fn new(config: ServerConfig, eth_node: Arc<EthNode>) -> Self {
        assert_eq!(
            tokio::runtime::Handle::current().runtime_flavor(),
            RuntimeFlavor::MultiThread,
            "Tests fail in single-threaded runtime because of blocking calls, use #[tokio::test(flavor = \"multi_thread\")]"
        );

        let root_dir = tempdir::TempDir::new("server").expect("Failed to create temp root dir");
        let api_port = PORT_POOL.lock().unwrap().get();
        let p2p_port = PORT_POOL.lock().unwrap().get();

        match std::env::var("COPY_DATA_FROM_DIR") {
            Ok(copy_data_from_dir) => {
                // copy both ${COPY_DATA_FROM_DIR}/db and ${COPY_DATA_FROM_DIR}/smirk to root_dir/db and root_dir/smirk
                // making sure that the directories exist in COPY_DATA_FROM_DIR
                for dir in &["db", "smirk"] {
                    let src = PathBuf::from(&copy_data_from_dir).join(dir).join("latest");
                    let dst = root_dir.path().join(dir).join("latest");
                    std::fs::create_dir_all(&dst).unwrap();
                    for entry in std::fs::read_dir(&src).unwrap() {
                        let entry = entry.unwrap();
                        let path = entry.path();
                        let file_name = path.file_name().unwrap();
                        let dst_path = dst.join(file_name);
                        std::fs::copy(&path, &dst_path).unwrap();
                    }
                }
            }
            Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => panic!("COPY_DATA_FROM_DIR has invalid unicode"),
        }

        // let public_key = secp256k1::SecretKey::from_slice(&config.secret_key)
        //     .unwrap()
        //     .public_key(secp256k1::SECP256K1);
        // let public_key_bytes = public_key.serialize_uncompressed();
        // let public_key_bytes = TryInto::<[u8; 64]>::try_into(&public_key_bytes[1..]).unwrap();
        // let address = public_to_address(&public_key_bytes.into());

        let (stdout_sender, stdout) = mpsc::channel();
        let (stderr_sender, stderr) = mpsc::channel();

        Self {
            process: None,
            root_dir,
            client: reqwest::Client::new(),
            keep_port_after_drop: config.keep_port_after_drop,
            safe_eth_height_offset: config.safe_eth_height_offset,
            secret_key: config.secret_key,
            rollup_contract_addr: config.rollup_contract,
            // address,
            peers: vec![Peer {
                p2p_port,
                // _address: address,
            }],
            prover: config.mock_prover,
            api_port,
            p2p_port,
            eth_node,
            stdout,
            stdout_sender: Some(stdout_sender),
            stderr,
            stderr_sender: Some(stderr_sender),
            output_readers: Vec::new(),
        }
    }

    fn base_url(&self) -> Url {
        format!("http://localhost:{}", self.api_port)
            .parse()
            .unwrap()
    }

    fn to_peer(&self) -> Peer {
        Peer {
            p2p_port: self.p2p_port,
            // _address: self.address,
        }
    }

    fn set_peers(&mut self, peers: &[Peer]) {
        self.peers = peers.to_vec();
    }

    fn run(&mut self, log_output: Option<bool>) {
        let mut command = Command::new(find_binary());

        command
            .arg("--db-path")
            .arg(self.root_dir.path().join("db"));
        command
            .arg("--smirk-path")
            .arg(self.root_dir.path().join("smirk"));
        command
            .arg("--rpc-laddr")
            .arg(format!("127.0.0.1:{}", self.api_port));
        command
            .arg("--p2p-laddr")
            .arg(format!("/ip4/127.0.0.1/tcp/{}", self.p2p_port));
        command
            .arg("--secret-key")
            .arg(format!("0x{}", hex::encode(self.secret_key)));
        command
            .arg("--rollup-contract-addr")
            .arg(format!("0x{:x}", self.rollup_contract_addr));
        command.arg("--eth-rpc-url").arg(self.eth_node.rpc_url());

        command.arg("--p2p-dial").arg(
            self.peers
                .iter()
                .map(|p| format!("/ip4/127.0.0.1/tcp/{}", p.p2p_port))
                .collect::<Vec<_>>()
                .join(","),
        );

        command.arg("--mode").arg(if self.prover {
            "mock-prover"
        } else {
            "validator"
        });

        command.env(
            "POLY_SAFE_ETH_HEIGHT_OFFSET",
            self.safe_eth_height_offset.to_string(),
        );

        let should_log = log_output.unwrap_or(
            std::env::var("LOG_NODE_OUTPUT")
                .map(|v| v == "1")
                .unwrap_or(false),
        );
        let output_piped = if !should_log {
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            true
        } else {
            false
        };

        let mut process = command.spawn().expect("Failed to start node");

        let stdout_sender = self.stdout_sender.take().unwrap();
        let stderr_sender = self.stderr_sender.take().unwrap();

        if output_piped {
            let mut stdout = process.stdout.take().unwrap();
            let mut stderr = process.stderr.take().unwrap();

            self.output_readers.push(std::thread::spawn(move || {
                let mut text = Vec::<u8>::new();
                stdout.read_to_end(&mut text).unwrap();

                let text = String::from_utf8_lossy(&text);
                let _ = stdout_sender.send(text.to_string());
            }));

            self.output_readers.push(std::thread::spawn(move || {
                let mut text = Vec::<u8>::new();
                stderr.read_to_end(&mut text).unwrap();

                let text = String::from_utf8_lossy(&text);
                let _ = stderr_sender.send(text.to_string());
            }));
        }

        println!(
            "Node started: {}; Base URL: {}",
            process.id(),
            self.base_url()
        );

        self.process = Some(process);
    }

    fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.kill().expect("Failed to kill node");
            process.wait().expect("Failed to wait for node to exit");

            for reader in self.output_readers.drain(..) {
                reader.join().unwrap();
            }

            if std::thread::panicking() {
                // If a test failed, print the last 10 lines of output so we can debug node errors
                let stdout = self
                    .stdout
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                let recent_stdout = stdout
                    .lines()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .fold(String::new(), |acc, line| acc + line + "\n");

                let stderr = self
                    .stderr
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                let recent_stderr = stderr
                    .lines()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .fold(String::new(), |acc, line| acc + line + "\n");

                eprintln!("Last 10 lines of node stdout:\n{recent_stdout}\n-------");
                eprintln!("Last 10 lines of node stderr:\n{recent_stderr}\n-------");

                let random_id = rand::random::<u32>();
                let logs_file_path = std::env::temp_dir().join(format!(
                    "node-pid-{}-port-{}-logs-{}",
                    process.id(),
                    self.api_port,
                    random_id
                ));

                std::fs::create_dir(&logs_file_path).unwrap();

                std::fs::write(logs_file_path.join("node-stdout.log"), stdout).unwrap();
                std::fs::write(logs_file_path.join("node-stderr.log"), stderr).unwrap();

                eprintln!(
                    "Full stdout saved to {}",
                    logs_file_path.join("node-stdout.log").display()
                );
                eprintln!(
                    "Full stderr saved to {}",
                    logs_file_path.join("node-stderr.log").display()
                );
            }
        }
    }

    // fn reset_db(&mut self) {
    //     std::fs::remove_dir_all(self.root_dir.path().join("db")).unwrap();
    // }

    // fn reset_smirk(&mut self) {
    //     std::fs::remove_dir_all(self.root_dir.path().join("smirk")).unwrap();
    // }

    async fn setup_and_wait(config: ServerConfig, eth_node: Arc<EthNode>) -> Self {
        let mut server = Self::new(config, eth_node);
        server.run(None);
        server.wait().await.expect("Failed to wait for server");
        server
    }

    async fn wait_for_healthy(&self) -> Result<(), Box<dyn std::error::Error>> {
        let time_between_requests = std::time::Duration::from_millis(100);
        let max_retries = 10_000 / time_between_requests.as_millis() as usize;

        let mut retry = 0;
        loop {
            let is_last_retry = retry == max_retries - 1;

            let req = self
                .client
                .get(self.base_url().join("/v0/health").unwrap())
                .build()
                .unwrap();

            match self.client.execute(req).await {
                Ok(res) if res.status().is_success() => return Ok(()),
                Ok(res) if is_last_retry => {
                    return Err(format!("Failed to get health: {}", res.status()).into())
                }
                Ok(_) => {}
                Err(err) if is_last_retry => return Err(err.into()),
                Err(_) => {}
            }

            tokio::time::sleep(time_between_requests).await;
            retry += 1;
        }
    }

    async fn wait(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.wait_for_healthy().await?;

        Ok(())
    }

    #[allow(dead_code)]
    async fn wait_to_notice_sync(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.wait_for_healthy().await?;

        // When a node starts up, it doesn't know if it's out of sync yet,
        // so /v0/health returns 200 OK, but practically the node is not
        // ready to serve requests yet. So we wait a second to work around that.
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        self.wait_for_healthy().await?;

        Ok(())
    }

    // async fn wait_for_height(&self, min_height: u64) -> Result<(), Error> {
    //     loop {
    //         let height = self.height().await?;
    //         if height.height > min_height {
    //             return Ok(());
    //         }
    //         tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    //     }
    // }

    pub async fn transaction(
        &self,
        snark_witness: &SnarkWitness,
    ) -> Result<TransactionResp, Error> {
        let res = self
            .client
            .post(self.base_url().join("/v0/transaction").unwrap())
            .json(&json!({
                "snark": snark_witness,
            }))
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json::<TransactionResp>().await.unwrap())
    }

    pub async fn height(&self) -> Result<HeightResp, Error> {
        let res = self
            .client
            .get(self.base_url().join("/v0/height").unwrap())
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json::<HeightResp>().await.unwrap())
    }

    pub async fn merkle(&self, commitments: &[Element]) -> Result<MerklePathResponse, Error> {
        let res = self
            .client
            .get(self.base_url().join("/v0/merkle").unwrap())
            .query(&[(
                "commitments",
                commitments
                    .iter()
                    .map(|e| e.to_hex())
                    .collect::<Vec<_>>()
                    .join(","),
            )])
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json::<MerklePathResponse>().await.unwrap())
    }

    pub async fn element(&self, element: Element) -> Result<ElementResponse, Error> {
        let res = self
            .client
            .get(
                self.base_url()
                    .join(&format!("/v0/elements/{}", element.to_hex()))
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json::<ElementResponse>().await.unwrap())
    }

    pub async fn list_blocks(&self, query: &ListBlocksQuery) -> Result<ListBlocksResponse, Error> {
        let res = self
            .client
            .get(self.base_url().join("/v0/blocks").unwrap())
            .query(&query)
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json::<ListBlocksResponse>().await.unwrap())
    }

    pub async fn list_transactions(
        &self,
        query: &ListTxnsQuery,
    ) -> Result<ListTransactionsResponse, Error> {
        let res = self
            .client
            .get(self.base_url().join("/v0/transactions").unwrap())
            .query(&query)
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json().await.unwrap())
    }

    pub async fn get_transaction(&self, hash: CryptoHash) -> Result<GetTransactionResponse, Error> {
        let res = self
            .client
            .get(
                self.base_url()
                    .join(&format!("/v0/transactions/{hash}"))
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json().await.unwrap())
    }

    pub async fn get_block(&self, identifier: &str) -> Result<BlockWithInfo, Error> {
        let res = self
            .client
            .get(
                self.base_url()
                    .join(&format!("/v0/blocks/{identifier}"))
                    .unwrap(),
            )
            .send()
            .await
            .unwrap();

        if !res.status().is_success() {
            let err = res.json::<Error>().await.unwrap();
            return Err(err);
        }

        Ok(res.json().await.unwrap())
    }
}

fn mint_with_note<'m, 't>(
    rollup: &'m RollupContract,
    _usdc: &'m USDCContract,
    server: &'t Server,
    note: Note,
) -> (
    impl Future<Output = Result<(), contracts::Error>> + 'm,
    impl Future<Output = Result<TransactionResp, Error>> + 't,
) {
    let utxo = zk_circuits::data::Utxo::<MERKLE_TREE_DEPTH>::new_mint(note.clone());
    let snark = cache_utxo_proof("mint", &utxo);

    let mint = Mint::new([note.clone()]);
    let proof = mint.evm_proof(ParameterSet::Eight).unwrap();

    (
        async move {
            let tx = rollup
                .mint(&proof, &note.commitment(), &note.value(), &note.source())
                .await?;

            while rollup
                .client
                .client()
                .eth()
                .transaction_receipt(tx)
                .await
                .unwrap()
                .map_or(true, |r| r.block_number.is_none())
            {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            Ok(())
        },
        async move { server.transaction(&snark).await },
    )
}

fn mint<'m, 't>(
    rollup: &'m RollupContract,
    usdc: &'m USDCContract,
    server: &'t Server,
    address: Element,
    value: Element,
) -> (
    Note,
    impl Future<Output = Result<(), contracts::Error>> + 'm,
    impl Future<Output = Result<TransactionResp, Error>> + 't,
) {
    let note = Note::restore(address, Element::new(0), value, Element::new(0));

    let (eth_tx, rpc_tx) = mint_with_note(rollup, usdc, server, note.clone());

    (note, eth_tx, rpc_tx)
}

fn burn<'m, 't>(
    rollup: &'m RollupContract,
    server: &'t Server,
    note: &'m InputNote<MERKLE_TREE_DEPTH>,
    to: &'m Address,
    via_router: bool,
) -> (
    impl Future<Output = Result<(), contracts::Error>> + 'm,
    impl Future<Output = Result<TransactionResp, Error>> + 't,
) {
    let utxo =
        zk_circuits::data::Utxo::<MERKLE_TREE_DEPTH>::new_burn(note.clone(), note.recent_root());
    let snark = cache_utxo_proof("burn", &utxo);

    (
        async move {
            let tx = if via_router {
                let router = Address::from_str("4a679253410272dd5232b3ff7cf5dbb88f295319").unwrap();
                let return_address =
                    Address::from_str("0000000000000000000000000000000000000001").unwrap();
                let usdc_address = rollup.usdc().await.unwrap();

                let mut router_calldata =
                    keccak256(b"burnToAddress(address,address,uint256)")[0..4].to_vec();
                router_calldata.extend_from_slice(&web3::ethabi::encode(&[
                    usdc_address.into_token(),
                    to.into_token(),
                    convert_element_to_h256(&note.value()).into_token(),
                ]));

                let msg = web3::ethabi::encode(&[
                    Token::Address(router),
                    Token::Bytes(router_calldata.clone()),
                    Token::Address(return_address),
                ]);

                let mut msg_hash = keccak256(&msg);
                // Bn256 can't fit the full hash, so we remove the first 3 bits
                msg_hash[0] &= 0x1f; // 0b11111

                let burn = BurnTo {
                    notes: [note.note().clone()],
                    secret_key: note.secret_key(),
                    to_address: Element::from_be_bytes(msg_hash),
                    kind: Element::ONE,
                };

                rollup
                    .burn_to_router(
                        &Element::ONE,
                        &Element::from_be_bytes(msg_hash),
                        &burn.evm_proof(ParameterSet::Nine).unwrap(),
                        &note.nullifer(),
                        &note.value(),
                        &note.source(),
                        &burn.signature(note.note()),
                        &router,
                        &router_calldata,
                        &return_address,
                    )
                    .await?
            } else {
                let burn = {
                    let notes = [note.note().clone()];
                    let secret_key = note.secret_key();
                    let to_address = convert_h160_to_element(to);
                    Burn {
                        notes,
                        secret_key,
                        to_address,
                    }
                };

                rollup
                    .burn(
                        to,
                        &burn.evm_proof(ParameterSet::Nine).unwrap(),
                        &note.nullifer(),
                        &note.value(),
                        &note.source(),
                        &burn.signature(note.note()),
                    )
                    .await?
            };

            while rollup
                .client
                .client()
                .eth()
                .transaction_receipt(tx)
                .await
                .unwrap()
                .map_or(true, |r| r.block_number.is_none())
            {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            Ok(())
        },
        async move { server.transaction(&snark).await },
    )
}

async fn rollup_contract(addr: Address, eth_node: &EthNode) -> RollupContract {
    let client = contracts::Client::new(&eth_node.rpc_url(), None);
    RollupContract::load(
        client,
        &hex::encode(addr.as_bytes()),
        SecretKey::from_str("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn usdc_contract(rollup: &RollupContract, eth_node: &EthNode) -> USDCContract {
    let usdc_addr = rollup.usdc().await.unwrap();

    let client = contracts::Client::new(&eth_node.rpc_url(), None);
    USDCContract::load(
        client,
        &hex::encode(usdc_addr.as_bytes()),
        SecretKey::from_str("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
            .unwrap(),
    )
    .await
    .unwrap()
}

fn cache_proof(name: &str, f: impl FnOnce() -> SnarkWitness) -> SnarkWitness {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/cache/{name}.proof"));

    if path.exists() {
        let proof = std::fs::read(&path).unwrap();
        return SnarkWitness::from_bytes(&proof).unwrap();
    }

    let proof = f();
    std::fs::write(&path, proof.to_bytes().unwrap()).unwrap();

    proof
}

fn hash_utxo(utxo: &zk_circuits::data::Utxo<MERKLE_TREE_DEPTH>) -> [u8; 32] {
    let utxo_hash = sha3::Sha3_256::digest(serde_json::to_string(&utxo).unwrap().as_bytes());
    utxo_hash.into()
}

fn cache_utxo_proof(name: &str, utxo: &zk_circuits::data::Utxo<MERKLE_TREE_DEPTH>) -> SnarkWitness {
    cache_proof(
        &format!("utxo-{}-{}", name, hex::encode(hash_utxo(utxo))),
        || SnarkWitness::V1(utxo.snark(CircuitKind::Utxo).unwrap().to_witness()),
    )
}
