//! Signs Taproot Transactions using Nostr keys.

use bitcoin::{
    Script, TapLeafHash, TapSighashType, Transaction, TxOut, Witness,
    hashes::Hash,
    key::TapTweak,
    sighash::{Prevouts, SighashCache},
    taproot::{self, ControlBlock, LeafVersion},
};
use dioxus::logger::tracing::trace;
use nostr::key::{PublicKey as NostrPublicKey, SecretKey as NostrSecretKey};
use secp256k1::{Message, SECP256K1};

use crate::{
    error::Error,
    scripts::{EscrowScript, escrow_scripts},
};

/// Signs a [`Transaction`] with the given [`NostrSecretKey`].
///
/// It must be a P2TR key path spend transaction with a single input as the 0th vout.
pub fn sign_resolution_tx(
    transaction: &Transaction,
    nsec: &NostrSecretKey,
    prevout: TxOut,
) -> Transaction {
    // Parse nsec to a bitcoin secret key.
    let keypair = nsec.keypair(SECP256K1);

    let mut sighasher = SighashCache::new(transaction);
    let sighash_type = TapSighashType::All;
    let sighash = sighasher
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), sighash_type)
        .expect("must create sighash");
    let message = Message::from_digest(*sighash.as_byte_array());
    // P2TR key path spend.
    let tweaked = keypair.tap_tweak(SECP256K1, None);
    let signature = SECP256K1.sign_schnorr(&message, &tweaked.to_inner());
    let signature = taproot::Signature {
        signature,
        sighash_type,
    };
    trace!(signature = %signature.signature, txid = %transaction.compute_txid(), "Signature resolution transaction");
    let mut transaction = transaction.clone();
    transaction.input[0].witness = Witness::p2tr_key_spend(&signature);
    transaction
}

/// Signs an escrow P2TR [`Transaction`], given an input `index` using a [`NostrSecretKey`].
///
/// The input is signed using the provided [`NostrSecretKey`], `prevouts`, and [`ScriptBuf`] locking script.
#[allow(clippy::too_many_arguments)]
pub fn sign_escrow_tx(
    tx: &Transaction,
    index: usize,
    nsec: &NostrSecretKey,
    npub_1: &NostrPublicKey,
    npub_2: &NostrPublicKey,
    npub_arbitrator: Option<&NostrPublicKey>,
    timelock_duration: Option<u32>,
    prevouts: Vec<TxOut>,
    escrow_script: EscrowScript,
) -> Result<taproot::Signature, Error> {
    // Parse nsec to a bitcoin secret key.
    let keypair = nsec.keypair(SECP256K1);

    // get which escrow type.
    let locking_script = escrow_scripts(
        npub_1,
        npub_2,
        npub_arbitrator,
        timelock_duration,
        escrow_script,
    )?;
    trace!(%index, locking_script = %locking_script.to_asm_string(), "escrow locking script");
    let leaf_hash = TapLeafHash::from_script(locking_script.as_script(), LeafVersion::TapScript);

    // TODO: This needs to follow the annoying BIP-342 extension:
    //       <https://github.com/bitcoin/bips/blob/master/bip-0342.mediawiki#signature-validation>
    //       <https://docs.rs/bitcoin/latest/bitcoin/sighash/struct.SighashCache.html#method.taproot_encode_signing_data_to>
    let sighash_type = TapSighashType::All;
    let mut sighash_cache = SighashCache::new(tx);
    let sighash = sighash_cache
        .taproot_script_spend_signature_hash(
            index,
            &Prevouts::All(&prevouts),
            leaf_hash,
            sighash_type,
        )
        .unwrap();
    let message = Message::from_digest(*sighash.as_byte_array());
    let signature = SECP256K1.sign_schnorr(&message, &keypair);
    let signature = taproot::Signature {
        signature,
        sighash_type,
    };
    trace!(%index, %signature.signature, txid = %tx.compute_txid(), "Signature escrow transaction");

    Ok(signature)
}

/// Types of escrow transactions.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum EscrowType<'a> {
    /// Collaborative escrow transaction.
    ///
    /// No timelocks and no arbitrator.
    Collaborative {
        participant_1: &'a NostrPublicKey,
        participant_2: &'a NostrPublicKey,
    },

    /// Dispute escrow transaction.
    ///
    /// Timelocked and with an arbitrator.
    Dispute {
        participant_1: &'a NostrPublicKey,
        participant_2: &'a NostrPublicKey,
        arbitrator: &'a NostrPublicKey,
    },
}

/// Combine one multiple [`schnorr::Signature`]s into a single [`Transaction`] input.
pub fn combine_signatures(
    mut transaction: Transaction,
    index: usize,
    signatures: Vec<&taproot::Signature>,
    locking_script: &Script,
    control_block: ControlBlock,
) -> Transaction {
    // Push signatures in order
    for signature in signatures {
        transaction.input[index].witness.push(signature.serialize());
    }

    // Push locking script
    transaction.input[index]
        .witness
        .push(locking_script.to_bytes());

    // Push control block
    transaction.input[index]
        .witness
        .push(control_block.serialize());

    transaction
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use bitcoin::{
        Amount, BlockHash, Network, OutPoint, TxIn, absolute, consensus, hex::DisplayHex,
        transaction,
    };

    use corepc_node::Node;
    use dioxus::logger::tracing::debug;
    use nostr::nips::nip21::NostrURI;
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

    use crate::{
        scripts::{escrow_address, escrow_spend_info},
        util::npub_to_address,
    };

    use super::*;

    static COINBASE_AMOUNT: LazyLock<Amount> = LazyLock::new(|| Amount::from_btc(50.0).unwrap());
    const FEE: Amount = Amount::from_sat(1_000);
    static MULTISIG_AMOUNT: LazyLock<Amount> = LazyLock::new(|| *COINBASE_AMOUNT - FEE);
    static ESCROW_AMOUNT: LazyLock<Amount> = LazyLock::new(|| *MULTISIG_AMOUNT - FEE);
    const COINBASE_MATURITY: usize = 101;

    // Generated by https://nostrtool.com
    // const NSEC_1: &str = "nsec1hufm8kzq0c4l9zsja7daynm47mfq2fkn38cm38yrpjmv6zctz2ysjmqw36";
    // const NPUB_1: &str = "npub1nckhhhcxm8usszvxt6yku6efp4fpay3saglx6yhtu8pfv3kdqhqsfn0vd7";
    // const NSEC_2: &str = "nsec1svda3gyta75ny0t7aqqv9ldh0hazt89qc48jjgw8wkv5wy9w6fgq34wv4z";
    // const NPUB_2: &str = "npub1xy4xk87gglf4psv3lr7aymvs09e44fq0zxcf6kc43lawusvz3cts270an7";

    fn generate_nostr_keys() -> (NostrSecretKey, NostrPublicKey) {
        let nsec = NostrSecretKey::generate();
        let npub: NostrPublicKey = nsec.public_key(SECP256K1).x_only_public_key().0.into();
        trace!(derived_npub = %npub.to_nostr_uri().unwrap());
        (nsec, npub)
    }

    #[test]
    fn sign_collaborative_tx_flow() {
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(EnvFilter::from_default_env())
            .init();

        // Setup regtest node and clients.
        let bitcoind = Node::from_downloaded().unwrap();
        let btc_client = &bitcoind.client;

        // Get network.
        let network = btc_client
            .get_blockchain_info()
            .expect("must get blockchain info")
            .chain;
        let network = network.parse::<Network>().expect("network must be valid");

        // Generate nsec and npub.
        let (nsec_1, npub_1) = generate_nostr_keys();
        let (nsec_2, npub_2) = generate_nostr_keys();
        // Get the xonly pks.
        let xonly_1 = nsec_1.x_only_public_key(SECP256K1).0;
        let xonly_2 = nsec_2.x_only_public_key(SECP256K1).0;
        trace!(%xonly_1, %xonly_2, "xonly pks");

        // Fund a SegWit-v1 P2TR address from the npub.
        // Mine until maturity (101 blocks in Regtest).
        let funded_address = npub_to_address(&npub_1, network).unwrap();
        trace!(%funded_address, "Funded address");
        let coinbase_block = btc_client
            .generate_to_address(COINBASE_MATURITY, &funded_address)
            .expect("must be able to generate blocks")
            .0
            .first()
            .expect("must be able to get the blocks")
            .parse::<BlockHash>()
            .expect("must parse");
        let coinbase_txid = btc_client
            .get_block(coinbase_block)
            .expect("must be able to get coinbase block")
            .coinbase()
            .expect("must be able to get the coinbase transaction")
            .compute_txid();

        // Send to the 2-of-2 multisig address.
        let escrow_address = escrow_address(&npub_1, &npub_2, None, None, network).unwrap();
        trace!(%escrow_address, "Escrow address");

        // Create the transaction.
        let funding_input = OutPoint {
            txid: coinbase_txid,
            vout: 0,
        };
        let inputs = vec![TxIn {
            previous_output: funding_input,
            ..Default::default()
        }];
        let outputs = vec![TxOut {
            value: *MULTISIG_AMOUNT,
            script_pubkey: escrow_address.script_pubkey(),
        }];
        let unsigned = Transaction {
            version: transaction::Version(2),
            input: inputs,
            output: outputs,
            lock_time: absolute::LockTime::ZERO,
        };
        trace!(transaction=%consensus::serialize(&unsigned).as_hex(), "Unsigned funding transaction");

        // Sign the first input using Sighashes
        let prevout = TxOut {
            value: *COINBASE_AMOUNT,
            script_pubkey: funded_address.script_pubkey(),
        };
        let signed = sign_resolution_tx(&unsigned, &nsec_1, prevout);
        trace!(transaction=%consensus::serialize(&signed).as_hex(), "Signed funding");

        // Test if the transaction is valid.
        let result = btc_client.send_raw_transaction(&signed);
        assert!(result.is_ok());
        let txid = result.unwrap().txid().unwrap();
        assert_eq!(txid, signed.compute_txid());
        debug!(%txid, "Sent to the escrow address");
        // Mine 1 block to mine the transaction
        btc_client.generate_to_address(1, &funded_address).unwrap();

        // Spend from the escrow address.
        let final_address = btc_client.new_address().unwrap();
        let unsigned = Transaction {
            version: transaction::Version(2),
            input: vec![TxIn {
                previous_output: OutPoint { txid, vout: 0 },
                ..Default::default()
            }],
            output: vec![TxOut {
                value: *ESCROW_AMOUNT,
                script_pubkey: final_address.script_pubkey(),
            }],
            lock_time: absolute::LockTime::ZERO,
        };
        trace!(transaction=%consensus::serialize(&unsigned).as_hex(), "Unsigned escrow transaction");

        let script_pubkey = escrow_address.script_pubkey();
        let prevouts = TxOut {
            value: *ESCROW_AMOUNT,
            script_pubkey,
        };
        let sig_1 = sign_escrow_tx(
            &unsigned,
            0,
            &nsec_1,
            &npub_1,
            &npub_2,
            None,
            None,
            vec![prevouts.clone()],
            EscrowScript::A,
        )
        .unwrap();
        let sig_2 = sign_escrow_tx(
            &unsigned,
            0,
            &nsec_2,
            &npub_1,
            &npub_2,
            None,
            None,
            vec![prevouts.clone()],
            EscrowScript::A,
        )
        .unwrap();

        let locking_script = escrow_scripts(&npub_1, &npub_2, None, None, EscrowScript::A).unwrap();
        let script_ver = &(locking_script.clone(), LeafVersion::TapScript);
        let control_block = escrow_spend_info(&npub_1, &npub_2, None, None)
            .unwrap()
            .control_block(script_ver)
            .unwrap();
        let signed = combine_signatures(
            unsigned,
            0,
            vec![&sig_1, &sig_2],
            &locking_script,
            control_block,
        );
        trace!(transaction=%consensus::serialize(&signed).as_hex(), "Signed escrow");
        let result = btc_client.send_raw_transaction(&signed);
        assert!(result.is_ok());
    }
}

/*
#[test]
fn sign_p2wsh_dispute_no_arbitrator_tx_flow() {
    // Setup regtest node and clients.
    let bitcoind = Node::from_downloaded().unwrap();
    let btc_client = &bitcoind.client;

    // Get network.
    let network = btc_client
        .get_blockchain_info()
        .expect("must get blockchain info")
        .chain;
    let network = network.parse::<Network>().expect("network must be valid");

    // Generate the sk and pk
    let (nsec_1, npub_1) = generate_nostr_keys();
    let (nsec_2, npub_2) = generate_nostr_keys();
    let (_nsec_arb, npub_arb) = generate_nostr_keys();

    // Fund a SegWit-v0 address from the PublicKey.
    // Mine until maturity (101 blocks in Regtest).
    let funded_address = npub_to_address(&npub_1, network).unwrap();
    println!("Funded address: {}", funded_address);
    let coinbase_block = btc_client
        .generate_to_address(COINBASE_MATURITY, &funded_address)
        .expect("must be able to generate blocks")
        .0
        .first()
        .expect("must be able to get the blocks")
        .parse::<BlockHash>()
        .expect("must parse");
    let coinbase_txid = btc_client
        .get_block(coinbase_block)
        .expect("must be able to get coinbase block")
        .coinbase()
        .expect("must be able to get the coinbase transaction")
        .compute_txid();

    // Send to the 2-of-3 multisig address.
    // We're sending 49.999 and 0.001 will be fees.
    let timelock_duration = 10;
    let multisig_address =
        dispute_address(&[&npub_1, &npub_2], &npub_arb, timelock_duration, network);
    println!("Multisig address: {}", multisig_address);

    // Commutative check.
    assert_eq!(
        dispute_address(&[&npub_1, &npub_2], &npub_arb, timelock_duration, network,),
        dispute_address(&[&npub_1, &npub_2], &npub_arb, timelock_duration, network,),
    );

    // Create the transaction.
    let funding_input = OutPoint {
        txid: coinbase_txid,
        vout: 0,
    };
    let inputs = vec![TxIn {
        previous_output: funding_input,
        ..Default::default()
    }];
    let outputs = vec![TxOut {
        value: MULTISIG_AMOUNT,
        script_pubkey: multisig_address.script_pubkey(),
    }];
    let unsigned = Transaction {
        version: transaction::Version(2),
        input: inputs,
        output: outputs,
        lock_time: LockTime::ZERO,
    };
    println!(
        "Unsigned funding transaction: {}",
        consensus::serialize(&unsigned).as_hex()
    );

    // Sign the first input using Sighashes
    let spk = funded_address.script_pubkey();
    let sighash_type = EcdsaSighashType::All;
    let mut sighash_cache = SighashCache::new(unsigned);
    let sighash = sighash_cache
        .p2wpkh_signature_hash(0, &spk, COINBASE_AMOUNT, sighash_type)
        .unwrap();
    let message = Message::from(sighash);
    let btc_sk_1 = SecretKey::from_slice(&nsec_1.secret_bytes()).unwrap();
    let btc_pk_1 = npub_to_x_only_public_key(&npub_1).unwrap();
    let signature = SECP256K1.sign_ecdsa(&message, &btc_sk_1);
    // Update the witness stack
    let signature = ecdsa::Signature {
        signature,
        sighash_type,
    };
    *sighash_cache.witness_mut(0).unwrap() = Witness::p2wpkh(&signature, &btc_pk_1.inner);
    let signed_tx = sighash_cache.into_transaction();
    println!("Signed funding transaction: {:?}", signed_tx);

    // Test if the transaction is valid.
    let result = btc_client.send_raw_transaction(&signed_tx);
    assert!(result.is_ok());
    let txid = result.unwrap().txid().unwrap();
    assert_eq!(txid, signed_tx.compute_txid());
    println!("Transaction ID: {}", txid);
    // Mine 1 block to mine the transaction
    btc_client
        .generate_to_address(timelock_duration as usize, &funded_address)
        .unwrap();

    // Spend from the 2-of-3 dispute address.
    let final_address = btc_client.new_address().unwrap();
    let unsigned_tx = Transaction {
        version: transaction::Version(2),
        input: vec![TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            ..Default::default()
        }],
        output: vec![TxOut {
            value: ESCROW_AMOUNT,
            script_pubkey: final_address.script_pubkey(),
        }],
        lock_time: LockTime::ZERO,
    };
    let script_pubkey = multisig_address.script_pubkey();
    assert!(script_pubkey.is_p2wsh());
    println!("ScriptPubKey: {}", script_pubkey);

    let unlocking_script =
        dispute_unlocking_script(&[&npub_1, &npub_2], &npub_arb, timelock_duration);
    println!("Unlocking Script: {}", unlocking_script);

    // Commutative check.
    assert_eq!(
        dispute_unlocking_script(&[&npub_1, &npub_2], &npub_arb, timelock_duration,),
        dispute_unlocking_script(&[&npub_2, &npub_1], &npub_arb, timelock_duration,),
    );

    let sig_1 = sign_tx(
        unsigned_tx.clone(),
        0,
        &nsec_1,
        MULTISIG_AMOUNT,
        &unlocking_script,
    );
    let sig_2 = sign_tx(
        unsigned_tx.clone(),
        0,
        &nsec_2,
        MULTISIG_AMOUNT,
        &unlocking_script,
    );
    let signed_tx = combine_signatures_dispute_collaborative(
        unsigned_tx,
        0,
        vec![sig_1, sig_2],
        vec![npub_1, npub_2],
        unlocking_script,
    );
    assert!(signed_tx.input[0].witness.witness_script().is_some());
    println!(
        "Signed transaction: {}",
        consensus::serialize(&signed_tx).as_hex()
    );
    let result = btc_client.send_raw_transaction(&signed_tx);
    assert!(result.is_ok());
}

#[test]
fn sign_p2wsh_dispute_with_arbitrator_tx_flow_1() {
    env_logger::init();

    // Setup regtest node and clients.
    let bitcoind = Node::from_downloaded().unwrap();
    let btc_client = &bitcoind.client;

    // Get network.
    let network = btc_client
        .get_blockchain_info()
        .expect("must get blockchain info")
        .chain;
    let network = network.parse::<Network>().expect("network must be valid");

    // Get the PrivateKey and PublicKeys from constants.
    let sec_key1 = PRIVATE_KEY1_HEX
        .parse::<SecretKey>()
        .expect("must parse secret key");
    let private_key1 = PrivateKey::new(sec_key1, network);
    let public_key1 = private_key1.public_key(SECP256K1);
    assert_eq!(
        private_key1.public_key(SECP256K1).to_string(),
        PUBLIC_KEY1_HEX
    );
    let sec_key2 = PRIVATE_KEY2_HEX
        .parse::<SecretKey>()
        .expect("must parse secret key");
    let private_key2 = PrivateKey::new(sec_key2, network);
    let public_key2 = private_key2.public_key(SECP256K1);
    assert_eq!(
        private_key2.public_key(SECP256K1).to_string(),
        PUBLIC_KEY2_HEX
    );
    let sec_key_third = PRIVATE_KEY3_HEX
        .parse::<SecretKey>()
        .expect("must parse secret key");
    let private_key_third = PrivateKey::new(sec_key_third, network);
    let public_key_third = private_key_third.public_key(SECP256K1);
    assert_eq!(
        private_key_third.public_key(SECP256K1).to_string(),
        PUBLIC_KEY3_HEX
    );

    // Fund a SegWit-v0 address from the PublicKey.
    // Mine until maturity (101 blocks in Regtest).
    let compressed_pk: CompressedPublicKey = public_key1.try_into().unwrap();
    let funded_address = Address::p2wpkh(&compressed_pk, network);
    println!("Funded address: {}", funded_address);
    let coinbase_block = btc_client
        .generate_to_address(101, &funded_address)
        .expect("must be able to generate blocks")
        .0
        .first()
        .expect("must be able to get the blocks")
        .parse::<BlockHash>()
        .expect("must parse");
    let coinbase_txid = btc_client
        .get_block(coinbase_block)
        .expect("must be able to get coinbase block")
        .coinbase()
        .expect("must be able to get the coinbase transaction")
        .compute_txid();

    // Send to the 2-of-3 multisig address.
    // We're sending 49.999 and 0.001 will be fees.
    let multisig_amount = Amount::from_btc(49.999).unwrap();
    let timelock_duration = 10;
    let multisig_address = new_dispute_address(
        [public_key1, public_key2],
        public_key_third,
        timelock_duration,
        network,
    );
    assert_eq!(
        new_dispute_address(
            [public_key1, public_key2],
            public_key_third,
            timelock_duration,
            network,
        ),
        new_dispute_address(
            [public_key2, public_key1],
            public_key_third,
            timelock_duration,
            network,
        ),
    );
    println!("Multisig address: {}", multisig_address);

    // Create the transaction.
    let funding_input = OutPoint {
        txid: coinbase_txid,
        vout: 0,
    };
    let inputs = vec![TxIn {
        previous_output: funding_input,
        ..Default::default()
    }];
    let outputs = vec![TxOut {
        value: multisig_amount,
        script_pubkey: multisig_address.script_pubkey(),
    }];
    let unsigned = Transaction {
        version: transaction::Version(2),
        input: inputs,
        output: outputs,
        lock_time: LockTime::ZERO,
    };
    println!(
        "Unsigned funding transaction: {}",
        consensus::serialize(&unsigned).as_hex()
    );

    // Sign the first input using Sighashes
    let spk = funded_address.script_pubkey();
    let coinbase_amount = Amount::from_btc(50.0).unwrap();
    let sighash_type = EcdsaSighashType::All;
    let mut sighash_cache = SighashCache::new(unsigned);
    let sighash = sighash_cache
        .p2wpkh_signature_hash(0, &spk, coinbase_amount, sighash_type)
        .unwrap();
    let message = Message::from(sighash);
    let signature = SECP256K1.sign_ecdsa(&message, &private_key1.inner);
    // Update the witness stack
    let signature = ecdsa::Signature {
        signature,
        sighash_type,
    };
    *sighash_cache.witness_mut(0).unwrap() = Witness::p2wpkh(&signature, &public_key1.inner);
    let signed_tx = sighash_cache.into_transaction();
    println!("Signed funding transaction: {:?}", signed_tx);

    // Test if the transaction is valid.
    let result = btc_client.send_raw_transaction(&signed_tx);
    assert!(result.is_ok());
    let txid = result.unwrap().txid().unwrap();
    assert_eq!(txid, signed_tx.compute_txid());
    println!("Transaction ID: {}", txid);
    // Mine 1 block to mine the transaction
    btc_client
        .generate_to_address(timelock_duration as usize, &funded_address)
        .unwrap();

    // Spend from the 2-of-3 dispute address.
    let final_address = btc_client.new_address().unwrap();
    // Again 0.001 fees.
    let final_amount = Amount::from_btc(49.998).unwrap();
    let unsigned_tx = Transaction {
        version: transaction::Version(2),
        input: vec![TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            sequence: Sequence::from_consensus(timelock_duration),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: final_amount,
            script_pubkey: final_address.script_pubkey(),
        }],
        lock_time: LockTime::ZERO,
    };
    let script_pubkey = multisig_address.script_pubkey();
    assert!(script_pubkey.is_p2wsh());
    println!("ScriptPubKey: {}", script_pubkey);

    let unlocking_script = new_dispute_unlocking_script(
        [public_key1, public_key2],
        public_key_third,
        timelock_duration,
    );
    assert_eq!(
        new_dispute_unlocking_script(
            [public_key1, public_key2],
            public_key_third,
            timelock_duration,
        ),
        new_dispute_unlocking_script(
            [public_key2, public_key1],
            public_key_third,
            timelock_duration,
        ),
    );
    println!("Unlocking Script: {}", unlocking_script);
    let sig_1 = sign_tx(
        unsigned_tx.clone(),
        0,
        private_key1,
        multisig_amount,
        unlocking_script.clone(),
    );
    let sig_third = sign_tx(
        unsigned_tx.clone(),
        0,
        private_key_third,
        multisig_amount,
        unlocking_script.clone(),
    );
    let signed_tx = combine_signatures_dispute_arbitrator(
        unsigned_tx,
        0,
        vec![sig_1, sig_third],
        vec![public_key1, public_key_third],
        unlocking_script,
    );
    assert!(signed_tx.input[0].witness.witness_script().is_some());
    println!(
        "Signed transaction: {}",
        consensus::serialize(&signed_tx).as_hex()
    );
    let result = btc_client.send_raw_transaction(&signed_tx);
    assert!(result.is_ok());
}
*/
