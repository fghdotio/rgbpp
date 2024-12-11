//! RGBPP lock
//!
//! Heap config (fixed size 4KB, dynamic size 1M, min block 64B)
//! https://github.com/nervosnetwork/ckb-std/blob/676455542258235a22f6f443b18e4b4d887a661a/src/global_alloc_macro/default_alloc.rs#L18

#![no_std]
#![cfg_attr(not(test), no_main)]

#[cfg(test)]
extern crate alloc;

use alloc::vec::Vec;
#[cfg(not(test))]
use ckb_std::default_alloc;
use ckb_std::{
    ckb_constants::Source,
    ckb_types::{
        packed::{Byte32, TransactionReader},
        prelude::*,
    },
    high_level::{
        load_cell_lock, load_cell_type_hash, load_script, load_transaction, load_witness_args,
        QueryIter,
    },
};
use rgbpp_core::{
    bitcoin::{self, parse_btc_tx, BTCTx, Digest, Sha256, MIN_BTC_TIME_LOCK_AFTER},
    ensure, ensure_eq,
    error::Error,
    on_chain::{bitcoin_light_client::check_btc_tx_exists, utils::*},
    rgbpp::{check_btc_time_lock, check_utxo_seal, is_btc_time_lock},
    schemas::rgbpp::*,
    utils::is_script_code_equal,
};
#[cfg(not(test))]
ckb_std::entry!(program_entry);
#[cfg(not(test))]
default_alloc!(4 * 1024, 1024 * 1024, 64);

pub fn program_entry() -> i8 {
    match main() {
        Ok(_) => 0,
        Err(err) => {
            let err_code = err as i8;
            ckb_std::debug!("failed because {}", err_code);
            err_code
        }
    }
}

fn main() -> Result<(), Error> {
    // parse config and witness
    let lock_args = {
        let rgbpp_lock = load_script()?;
        RGBPPLock::from_slice(&rgbpp_lock.args().raw_data()).map_err(|_| Error::BadRGBPPLock)?
    };
    let ckb_tx = load_transaction()?;
    let ckb_tx_reader = ckb_tx.as_reader();
    let config = load_config::<RGBPPConfig>(&ckb_tx_reader)?;
    let unlock_witness = fetch_unlock_from_witness()?;

    // parse bitcoin transaction
    let raw_btc_tx = unlock_witness.btc_tx().raw_data();
    let btc_tx: BTCTx = parse_btc_tx(&raw_btc_tx)?;

    verify_unlock(
        &config,
        &lock_args,
        &unlock_witness,
        &btc_tx,
        &ckb_tx_reader,
    )?;
    verify_outputs(&config, &btc_tx)?;
    Ok(())
}

/// Verify outputs cells is protected with RGB++ lock
fn verify_outputs(config: &RGBPPConfig, btc_tx: &BTCTx) -> Result<(), Error> {
    let rgbpp_lock = load_script()?;
    for (index, type_hash) in QueryIter::new(load_cell_type_hash, Source::Output).enumerate() {
        // ignore non-type cells
        if type_hash.is_none() {
            continue;
        }

        let lock = load_cell_lock(index, Source::Output)?;
        // check RGB++ lock
        if is_script_code_equal(&lock, &rgbpp_lock) {
            // check new seal txid + index is valid
            let lock_args =
                RGBPPLock::from_slice(&lock.args().raw_data()).map_err(|_| Error::BadRGBPPLock)?;
            if check_utxo_seal(&lock_args, btc_tx) {
                continue;
            }
        }

        // check bitcoin time lock
        if is_btc_time_lock(config, &lock) {
            // check new seal txid + index is valid
            let lock_args = BTCTimeLock::from_slice(&lock.args().raw_data())
                .map_err(|_| Error::BadBTCTimeLock)?;
            if check_btc_time_lock(&lock_args, btc_tx, MIN_BTC_TIME_LOCK_AFTER) {
                continue;
            }
        }

        return Err(Error::OutputCellWithUnknownLock);
    }
    Ok(())
}

fn load_unlock(index: usize, source: Source) -> Result<RGBPPUnlock, Error> {
    let witness_args = load_witness_args(index, source)?;
    match witness_args.lock().to_opt() {
        Some(args) => {
            let unlock =
                RGBPPUnlock::from_slice(&args.raw_data()).map_err(|_| Error::BadRGBPPUnlock)?;
            Ok(unlock)
        }
        None => Err(Error::ItemMissing),
    }
}

/// Fetch unlock
///
/// In most cases, the RGBPPUnlock is located at group_input[0].lock.
///
/// For RGB++ cells which seal UTXOs in the same BTC transaction, the RGBPPUnlock is also the same.
/// Therefore, to reduce duplicated witness, we can pass an index to group_input[0].lock.
/// In such a situation, we load RGBPPUnlock from the index position.
fn fetch_unlock_from_witness() -> Result<RGBPPUnlock, Error> {
    let witness_args = load_witness_args(0, Source::GroupInput)?;
    match witness_args.lock().to_opt() {
        Some(args) if args.len() == 4 => {
            // we assume args represents index of witness if len is 4
            let index = u32::from_le_bytes(
                args.raw_data()[..]
                    .try_into()
                    .map_err(|_| Error::BadRGBPPUnlock)?,
            );
            // load unlock from index
            load_unlock(index as usize, Source::Input)
        }
        Some(args) => {
            // parse unlock
            let unlock =
                RGBPPUnlock::from_slice(&args.raw_data()).map_err(|_| Error::BadRGBPPUnlock)?;
            Ok(unlock)
        }
        None => Err(Error::ItemMissing),
    }
}

/// Verify unlock
fn verify_unlock(
    config: &RGBPPConfig,
    lock_args: &RGBPPLock,
    unlock_witness: &RGBPPUnlock,
    btc_tx: &BTCTx,
    ckb_tx: &TransactionReader,
) -> Result<(), Error> {
    // * 检查是否传入正确的 out_point
    // check bitcoin transaction inputs unlock RGB++ cell
    let expected_out_point: (Byte32, u32) = (lock_args.btc_txid(), lock_args.out_index().unpack());
    let is_found = btc_tx
        .inputs
        .iter()
        .any(|txin| txin.previous_output == expected_out_point);
    ensure!(is_found, Error::UtxoSealMismatch);

    // check bitcoin transaction exists in light client
    let btc_tx_proof = unlock_witness.btc_tx_proof().raw_data();
    check_btc_tx_exists(&config.btc_lc_type_hash(), &btc_tx.txid, 0, &btc_tx_proof)?;

    // verify commitment
    check_btc_tx_commitment(config, btc_tx, ckb_tx, unlock_witness)?;
    Ok(())
}

fn check_btc_tx_commitment(
    config: &RGBPPConfig,
    btc_tx: &BTCTx,
    ckb_tx: &TransactionReader,
    unlock_witness: &RGBPPUnlock,
) -> Result<(), Error> {
    let rgbpp_script = load_script().map_err(|_| Error::BadRGBPPLock)?;
    // 1. find BTC commitment
    let btc_commitment = bitcoin::extract_commitment(btc_tx).ok_or(Error::BadBtcCommitment)?;

    // 2. verify commitment extra data
    let raw_ckb_tx = ckb_tx.raw();
    let version: u16 = unlock_witness.version().unpack();
    let input_len: u8 = unlock_witness.extra_data().input_len().into();
    let output_len: u8 = unlock_witness.extra_data().output_len().into();
    ensure_eq!(version, 0, Error::UnknownCommitmentVersion);
    ensure!(input_len > 0, Error::BadBtcCommitment);
    ensure!(output_len > 0, Error::BadBtcCommitment);
    let inputs_are_committed = QueryIter::new(load_cell_type_hash, Source::Input)
        .skip(input_len.into())
        .all(|type_hash| type_hash.is_none());
    ensure!(inputs_are_committed, Error::CommitmentMismatch);

    let outputs_are_committed = raw_ckb_tx
        .outputs()
        .iter()
        .skip(output_len.into())
        .all(|output| output.type_().is_none());
    ensure!(outputs_are_committed, Error::CommitmentMismatch);

    // 3. gen commitment from current CKB transaction
    let mut hasher = Sha256::new();
    hasher.update(b"RGB++");
    hasher.update(version.to_le_bytes());
    hasher.update([input_len, output_len]);
    for input in raw_ckb_tx.inputs().iter().take(input_len.into()) {
        hasher.update(input.previous_output().as_slice());
    }
    for (output, data) in raw_ckb_tx
        .outputs()
        .iter()
        .zip(raw_ckb_tx.outputs_data().iter())
        .take(output_len.into())
    {
        let lock = output.lock().to_entity();
        if is_btc_time_lock(config, &lock) {
            let lock_args = BTCTimeLock::from_slice(&lock.args().raw_data())
                .map_err(|_| Error::BadBTCTimeLock)?
                .as_builder()
                .btc_txid(Byte32::default())
                .build();
            let lock = lock.as_builder().args(lock_args.as_bytes().pack()).build();
            let output = output.to_entity().as_builder().lock(lock).build();
            hasher.update(output.as_slice());
        } else if is_script_code_equal(&rgbpp_script, &lock) {
            let lock_args = RGBPPLock::from_slice(&lock.args().raw_data())
                .map_err(|_| Error::BadRGBPPLock)?
                .as_builder()
                .btc_txid(Byte32::default())
                .build();
            let lock = lock.as_builder().args(lock_args.as_bytes().pack()).build();
            let output = output.to_entity().as_builder().lock(lock).build();
            hasher.update(output.as_slice());
        } else {
            hasher.update(output.as_slice());
        }
        let data: Vec<u8> = data.raw_data().into();
        let data_len: u32 = data.len() as u32;
        hasher.update(data_len.to_le_bytes());
        hasher.update(&data);
    }

    // double sha256
    let commitment = bitcoin::sha2(&hasher.finalize()).pack();
    ensure_eq!(commitment, btc_commitment, Error::CommitmentMismatch);
    Ok(())
}
