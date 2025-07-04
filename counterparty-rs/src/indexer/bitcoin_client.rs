use std::cmp::min;
use std::collections::HashMap;
use std::iter::repeat;
use std::thread::JoinHandle;

use crate::b58::b58_encode;
use crate::utils::{script_to_address, script_to_address_legacy};
use bitcoin::{
    consensus::serialize,
    hashes::{hex::prelude::*, ripemd160, sha256, sha256d::Hash as Sha256dHash, Hash},
    opcodes::all::{
        OP_CHECKMULTISIG, OP_CHECKSIG, OP_EQUAL, OP_HASH160, OP_PUSHNUM_1, OP_PUSHNUM_2,
        OP_PUSHNUM_3, OP_RETURN,
    },
    script::Instruction::{Op, PushBytes},
    Block, BlockHash, Script, TxOut, Txid,
};

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use crypto::rc4::Rc4;
use crypto::symmetriccipher::SynchronousStreamCipher;

use crate::indexer::block::VinOutput;
use crate::indexer::rpc_client::{BatchRpcClient, BATCH_CLIENT};

use std::sync::Arc;

use serde_cbor::Value;

use super::{
    block::{
        Block as CrateBlock, ParsedVouts, PotentialDispenser, ToBlock, Transaction, Vin, Vout,
    },
    config::{Config, Mode},
    stopper::Stopper,
    types::{
        entry::{
            BlockAtHeightHasHash, BlockAtHeightSpentOutputInTx,
            ScriptHashHasOutputsInBlockAtHeight, ToEntry, TxInBlockAtHeight, WritableEntry,
        },
        error::Error,
        pipeline::{BlockHasEntries, BlockHasPrevBlockHash},
    },
    workers::new_worker_pool,
};

impl BlockHasEntries for Block {
    fn get_entries(&self, mode: Mode, height: u32) -> Vec<Box<dyn ToEntry>> {
        let hash = self.block_hash().as_byte_array().to_owned();
        let mut entries: Vec<Box<dyn ToEntry>> =
            vec![Box::new(WritableEntry::new(BlockAtHeightHasHash {
                height,
                hash,
            }))];
        if mode == Mode::Fetcher {
            return entries;
        }
        let mut script_hashes = HashMap::new();
        for tx in self.txdata.iter() {
            let entry = TxInBlockAtHeight {
                txid: tx.compute_txid().to_byte_array(),
                height,
            };
            entries.push(Box::new(WritableEntry::new(entry)));
            for i in tx.input.iter() {
                let entry = BlockAtHeightSpentOutputInTx {
                    txid: i.previous_output.txid.to_byte_array(),
                    vout: i.previous_output.vout,
                    height,
                };
                entries.push(Box::new(WritableEntry::new(entry)));
            }
            for o in tx.output.iter() {
                let script_hash = o.script_pubkey.script_hash().as_byte_array().to_owned();
                script_hashes.entry(script_hash).or_insert_with(|| {
                    let entry = ScriptHashHasOutputsInBlockAtHeight {
                        script_hash,
                        height,
                    };
                    entries.push(Box::new(WritableEntry::new(entry)));
                });
            }
        }
        entries
    }
}

fn arc4_decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut rc4 = Rc4::new(key);
    let mut result: Vec<u8> = repeat(0).take(data.len()).collect();
    rc4.process(data, &mut result);
    result
}


fn is_valid_segwit_script_legacy(script: &Script) -> bool {
    if let Some(Ok(PushBytes(pb))) = script.instructions().next() {
        return pb.is_empty();
    }
    false
}

fn is_valid_segwit_script(script: &Script) -> bool {
    if let Some(instruction) = script.instructions().next() {
        match instruction {
            Ok(bitcoin::blockdata::script::Instruction::PushBytes(pb)) => {
                return pb.is_empty();
            },
            Ok(inst) => {
                return format!("{:?}", inst).contains("OP_PUSHNUM_1");
            },
            Err(_) => {
                return false;
            }
        }
    }
    false
}

enum ParseOutput {
    Destination(String),
    Data(Vec<u8>),
}

impl ParseOutput {
    pub fn is_destination(&self) -> bool {
        matches!(self, ParseOutput::Destination(_))
    }
}

fn parse_vout(
    config: &Config,
    key: Vec<u8>,
    height: u32,
    txid: String,
    vi: usize,
    vout: &TxOut,
) -> Result<(ParseOutput, Option<PotentialDispenser>), Error> {
    let value = vout.value.to_sat();
    let is_p2sh = matches!(
        vout.script_pubkey
            .instructions()
            .collect::<Vec<_>>()
            .as_slice(),
        [Ok(Op(OP_HASH160)), Ok(PushBytes(_)), Ok(Op(OP_EQUAL))]
    );
    if vout.script_pubkey.is_op_return() {
        if let [Ok(Op(OP_RETURN)), Ok(PushBytes(pb))] = vout
            .script_pubkey
            .instructions()
            .collect::<Vec<_>>()
            .as_slice()
        {
            if config.taproot_support_enabled(height) {
                let bytes = pb.as_bytes();
                if bytes == b"CNTRPRTY" {
                    return Ok((
                        ParseOutput::Data(bytes.to_vec()),
                        Some(PotentialDispenser {
                            destination: None,
                            value: None,
                        }),
                    ));
                }
            }
            let bytes = arc4_decrypt(&key, pb.as_bytes());
            if bytes.starts_with(&config.prefix) {
                return Ok((
                    ParseOutput::Data(bytes[config.prefix.len()..].to_vec()),
                    Some(PotentialDispenser {
                        destination: None,
                        value: None,
                    }),
                ));
            }
        } 
        return Err(Error::ParseVout(format!(
            "Encountered invalid OP_RETURN script | tx: {}, vout: {}",
            txid, vi
        )));

    } else if vout.script_pubkey.instructions().last() == Some(Ok(Op(OP_CHECKSIG))) {
        let instructions: Vec<_> = vout.script_pubkey.instructions().collect();
        if instructions.len() < 3 {
            return Err(Error::ParseVout(format!(
                "Encountered invalid OP_CHECKSIG script | tx: {}, vout: {}",
                txid, vi
            )));
        }
        let pb = match instructions.get(2) {
            Some(Ok(instruction)) => match instruction {
                Op(OP_PUSHNUM_1) => vec![1],
                PushBytes(bytes) => bytes.as_bytes().to_vec(),
                Op(op) => vec![op.to_u8()],
            },
            Some(Err(_)) => vec![],
            None => vec![],
        };
        let bytes = arc4_decrypt(&key, &pb);
        if bytes.len() >= config.prefix.len() && bytes[1..=config.prefix.len()] == config.prefix {
            let data_len = bytes[0] as usize;
            let data = bytes[1..=data_len].to_vec();
            return Ok((
                ParseOutput::Data(data[config.prefix.len()..].to_vec()),
                Some(PotentialDispenser {
                    destination: None,
                    value: Some(value),
                }),
            ));
        } else {
            let destination = b58_encode(
                config
                    .address_version
                    .clone()
                    .into_iter()
                    .chain(pb)
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            return Ok((
                ParseOutput::Destination(destination.clone()),
                Some(PotentialDispenser {
                    destination: Some(destination),
                    value: Some(value),
                }),
            ));
        }
    } else if vout.script_pubkey.instructions().last() == Some(Ok(Op(OP_CHECKMULTISIG))) {
        let mut chunks = Vec::new();
        #[allow(unused_assignments)]
        let mut signatures_required = 0;
        match vout
            .script_pubkey
            .instructions()
            .collect::<Vec<_>>()
            .as_slice()
        {
            [Ok(PushBytes(_pk0_pb)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(PushBytes(_pk3_pb)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 1;
                for pb in [pk1_pb, pk2_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(Op(OP_PUSHNUM_1)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(Op(OP_PUSHNUM_2)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 1;
                for pb in [pk1_pb, pk2_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(Op(OP_PUSHNUM_2)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(Op(OP_PUSHNUM_2)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 2;
                for pb in [pk1_pb, pk2_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            // legacy edge case
            [Ok(Op(OP_PUSHNUM_3)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(Op(OP_PUSHNUM_2)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 3;
                for pb in [pk1_pb, pk2_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(Op(OP_PUSHNUM_1)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(PushBytes(pk3_pb)), Ok(Op(OP_PUSHNUM_3)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 1;
                for pb in [pk1_pb, pk2_pb, pk3_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(PushBytes(_pk0_pb)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(PushBytes(pk3_pb)), Ok(PushBytes(_pk4_pb)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 2;
                for pb in [pk1_pb, pk2_pb, pk3_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(Op(OP_PUSHNUM_2)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(PushBytes(pk3_pb)), Ok(Op(OP_PUSHNUM_3)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 2;
                for pb in [pk1_pb, pk2_pb, pk3_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            [Ok(Op(OP_PUSHNUM_3)), Ok(PushBytes(pk1_pb)), Ok(PushBytes(pk2_pb)), Ok(PushBytes(pk3_pb)), Ok(Op(OP_PUSHNUM_3)), Ok(Op(OP_CHECKMULTISIG))] =>
            {
                signatures_required = 3;
                for pb in [pk1_pb, pk2_pb, pk3_pb] {
                    chunks.push(pb.as_bytes().to_vec());
                }
            }
            _ => {
                return Err(Error::ParseVout(format!(
                    "Encountered invalid OP_MULTISIG script | tx: {}, vout: {}",
                    txid, vi
                )));
            }
        }
        let mut enc_bytes = Vec::new();
        for chunk in chunks.iter().take(chunks.len() - 1) {
            // (No data in last pubkey.)
            if chunk.len() < 2 {
                return Err(Error::ParseVout(format!(
                    "Encountered invalid OP_MULTISIG script | tx: {}, vout: {}",
                    txid, vi
                )));
            }
            enc_bytes.extend(chunk[1..chunk.len() - 1].to_vec()); // Skip sign byte and nonce byte.
        }
        let bytes = arc4_decrypt(&key, &enc_bytes);
        if bytes.len() >= config.prefix.len() && bytes[1..=config.prefix.len()] == config.prefix {
            let chunk_len = min(bytes[0] as usize, bytes.len() - 1);
            let chunk = bytes[1..=chunk_len].to_vec();
            return Ok((
                ParseOutput::Data(chunk[config.prefix.len()..].to_vec()),
                Some(PotentialDispenser {
                    destination: None,
                    value: Some(value),
                }),
            ));
        } else {
            let mut pub_key_hashes = chunks
                .into_iter()
                .map(|chunk| {
                    b58_encode(
                        &config
                            .address_version
                            .clone()
                            .into_iter()
                            .chain(
                                ripemd160::Hash::hash(sha256::Hash::hash(&chunk).as_byte_array())
                                    .as_byte_array()
                                    .to_vec(),
                            )
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            pub_key_hashes.sort();
            let pub_key_hashes_n_s = pub_key_hashes.len().to_string();
            let destination = [signatures_required.to_string()]
                .into_iter()
                .chain(pub_key_hashes.into_iter().chain([pub_key_hashes_n_s]))
                .collect::<Vec<_>>()
                .join("_");
            return Ok((
                ParseOutput::Destination(destination.clone()),
                Some(PotentialDispenser {
                    destination: Some(destination),
                    value: Some(value),
                }),
            ));
        }
    } else if is_p2sh && config.p2sh_address_supported(height) {
        if let [Ok(Op(OP_HASH160)), Ok(PushBytes(pb)), Ok(Op(OP_EQUAL))] = vout
            .script_pubkey
            .instructions()
            .collect::<Vec<_>>()
            .as_slice()
        {
            let destination = b58_encode(
                &config
                    .p2sh_address_version
                    .clone()
                    .into_iter()
                    .chain(pb.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
            );
            let mut potential_dispenser = Some(PotentialDispenser {
                destination: None,
                value: None,
            });
            if config.p2sh_dispensers_supported(height) {
                potential_dispenser = Some(PotentialDispenser {
                    destination: Some(destination.clone()),
                    value: Some(value),
                });
            }
            return Ok((ParseOutput::Destination(destination), potential_dispenser));
        }
        return Err(Error::ParseVout(format!(
            "Encountered invalid P2SH script | tx: {}, vout: {}",
            txid, vi
        )));
    } else if (config.segwit_supported(height) && is_valid_segwit_script_legacy(&vout.script_pubkey)) || 
                (config.taproot_support_enabled(height) && is_valid_segwit_script(&vout.script_pubkey)) || 
                (config.taproot_support_enabled(height) && vout.script_pubkey.is_p2tr()) {
        
         let destination = if config.taproot_support_enabled(height) {
            script_to_address(
                vout.script_pubkey.as_bytes().to_vec(),
                config.network.to_string().as_str(),
            )
        } else {
            script_to_address_legacy(
                vout.script_pubkey.as_bytes().to_vec(),
                config.network.to_string().as_str(),
            )
        }
        .map_err(|e| Error::ParseVout(format!("Segwit script to address failed: {}", e)))?;
        let mut potential_dispenser = Some(PotentialDispenser {
            destination: None,
            value: None,
        });
        if config.correct_segwit_txids_enabled(height) {
            potential_dispenser = Some(PotentialDispenser {
                destination: Some(destination.clone()),
                value: Some(value),
            });
        }
        return Ok((ParseOutput::Destination(destination), potential_dispenser));
    } else {
        return Err(Error::ParseVout(format!(
            "Unrecognized output type | tx: {}, vout: {}",
            txid, vi
        )));
    }
}

fn extract_data_from_witness(script: &Script) -> Result<Vec<u8>, Error> {
    let instructions: Vec<_> = script.instructions().collect();
    
    // Check if we have enough instructions for a valid envelope script
    if instructions.len() < 5 {
        return Err(Error::ParseVout("Invalid witness script: too few instructions".to_string()));
    }
    
    // Verify it's an envelope script with empty push bytes as equivalent to OP_FALSE
    let is_envelope = match (&instructions[0], &instructions[1], instructions.last()) {
        (Ok(PushBytes(pb)), Ok(Op(op2)), Some(Ok(Op(op3)))) if pb.is_empty() => {
            format!("{:?}", op2).contains("OP_IF") && format!("{:?}", op3).contains("OP_CHECKSIG")
        },
        (Ok(Op(op1)), Ok(Op(op2)), Some(Ok(Op(op3)))) => {
            (format!("{:?}", op1).contains("OP_FALSE") || format!("{:?}", op1).contains("OP_0")) && 
            format!("{:?}", op2).contains("OP_IF") && 
            format!("{:?}", op3).contains("OP_CHECKSIG")
        },
        _ => false
    };
    
    if !is_envelope {
        return Err(Error::ParseVout("Not an envelope script".to_string()));
    }
    
    // Check if this is an "ord" inscription
    let is_ord = instructions.len() >= 7 && 
        match (&instructions.get(2), &instructions.get(3)) {
            (Some(Ok(PushBytes(pb1))), Some(Ok(PushBytes(pb2)))) => {
                pb1.as_bytes() == b"ord" && 
                (pb2.as_bytes().len() == 1 && pb2.as_bytes()[0] == 7) // 7 for metaprotocol
            },
            _ => false
        };

    if is_ord {
        // Extract mime_type from the script (index 4)
        let mime_type = match &instructions.get(6) {
            Some(Ok(PushBytes(pb))) => {
                match std::str::from_utf8(pb.as_bytes()) {
                    Ok(mime) => mime.to_string(),
                    Err(_) => "".to_string(), // Default to empty string if decoding fails
                }
            },
            _ => "".to_string(), // Default to empty string if not found
        };
        
        // For ord inscriptions, collect all metadata chunks and description chunks
        let mut metadata_chunks = Vec::new();
        let mut description_chunks = Vec::new();
        
        let mut i = 7; // Skip protocol prefix elements
        let mut current_section = "none";
        
        // Process all instructions to collect metadata and description
        while i < instructions.len() - 3 { // Skip last 3 instructions: op_endif and checksig
            match &instructions[i] {
                Ok(PushBytes(marker)) => {
                    let marker_bytes = marker.as_bytes();
                    if marker_bytes.len() == 1 && marker_bytes[0] == 5 {
                        current_section = "metadata";
                        i += 1;
                        continue;
                    } else if (marker_bytes.len() == 1 && marker_bytes[0] == 0) || marker_bytes.is_empty() {
                        current_section = "description";
                        i += 1;
                        continue;
                    }
                },
                Ok(Op(op)) => {
                    // Vérifier si l'instruction est OP_0/OP_FALSE pour le marqueur de description
                    if format!("{:?}", op).contains("OP_0") || format!("{:?}", op).contains("OP_FALSE") {
                        current_section = "description";
                        i += 1;
                        continue;
                    }
                },
                _ => {}
            }

            // Collect the chunk if we're in a data section
            if current_section != "none" {
                if let Ok(PushBytes(data)) = &instructions[i] {
                    if current_section == "metadata" {
                        metadata_chunks.push(data.as_bytes().to_vec());
                    } else if current_section == "description" {
                        description_chunks.push(data.as_bytes().to_vec());
                    }
                }
            }
            
            i += 1;
        }
        
        // Combine all metadata chunks
        let mut combined_metadata = Vec::new();
        for chunk in metadata_chunks {
            combined_metadata.extend_from_slice(&chunk);
        }
        
        // Combine all description chunks
        let mut combined_description = Vec::new();
        for chunk in &description_chunks {
            combined_description.extend_from_slice(chunk);
        }
        
        // Always store descriptions as raw bytes
        let description_value = Value::Bytes(combined_description);
        
        // If we have metadata, use it directly
        if !combined_metadata.is_empty() {
            // First try to decode existing CBOR data
            match serde_cbor::from_slice::<Value>(&combined_metadata) {
                Ok(value) => {
                    // Extract message_type_id and create a modified value in one step
                    let (message_type_id, mut value_without_type_id) = match value {
                        Value::Array(mut arr) => {
                            if arr.is_empty() {
                                return Err(Error::ParseVout("CBOR array is empty, missing message_type_id".to_string()));
                            }
                            let type_id = arr.remove(0);
                            (type_id, Value::Array(arr))
                        },
                        _ => return Err(Error::ParseVout("Expected CBOR array, found different type".to_string())),
                    };
                    
                    // Ensure message_type_id is an integer
                    let type_id = match message_type_id {
                        Value::Integer(id) => id as u8,
                        _ => return Err(Error::ParseVout("message_type_id must be an integer".to_string())),
                    };
                    
                    // If there's a description, add it back to the data structure
                    if let Value::Array(ref mut arr) = value_without_type_id {
                        // Add the mime_type before the description
                        arr.push(Value::Text(mime_type));
                        
                        // Add the description if it's not empty
                        if !description_chunks.is_empty() {
                            arr.push(description_value);
                        }
                    }
                    
                    // Repack the message as CBOR
                    match serde_cbor::to_vec(&value_without_type_id) {
                        Ok(final_data) => {
                            // Create a Vec with just the message_type_id byte
                            let mut result = vec![type_id];
                            // Append the rest of the CBOR data
                            result.extend_from_slice(&final_data);
                            Ok(result)
                        },
                        Err(e) => Err(Error::ParseVout(format!("Failed to encode CBOR data: {}", e))),
                    }
                },
                Err(e) => {
                   Err(Error::ParseVout(format!("CBOR decode error: {}", e)))
                }
            }
        } else {
            // Neither metadata nor description found
            Err(Error::ParseVout("No data found in the ord inscription".to_string()))
        }
    } else {
        // Generic inscription - collect all data between OP_IF and OP_ENDIF
        let mut result_data = Vec::new();
        for i in 2..instructions.len() - 3 {
            if let Ok(PushBytes(bytes)) = &instructions[i] {
                result_data.extend_from_slice(bytes.as_bytes());
            }
        }
        return Ok(result_data);
    }
}

pub fn parse_transaction(
    tx: &bitcoin::Transaction,
    config: &Config,
    height: u32,
    parse_vouts: bool,
) -> Transaction {
    let tx_bytes = serialize(tx);
    let mut vins = Vec::new();
    let mut segwit = false;
    let mut vtxinwit: Vec<Vec<String>> = Vec::new();

    // Always process all inputs
    for (i, vin) in tx.input.iter().enumerate() {
        if !vin.witness.is_empty() {
            vtxinwit.push(
                vin.witness
                    .iter()
                    .map(|w| w.as_hex().to_string())
                    .collect::<Vec<_>>(),
            );
            segwit = true;
        } else {
            vtxinwit.push(Vec::new());
        }
    }

    let key = if !tx.input.is_empty() {
        let mut key = tx.input[0].previous_output.txid.to_byte_array().to_vec();
        key.reverse();
        key
    } else {
        Vec::new()
    };

    let mut vouts = Vec::new();
    let mut destinations = Vec::new();
    let mut fee = 0;
    let mut btc_amount = 0;
    let mut data = Vec::new();
    let mut is_reveal_tx = false;
    let mut commit_parent_txid = Txid::from_raw_hash(Sha256dHash::all_zeros());
    let mut commit_parent_vout = 0;
    let mut potential_dispensers = Vec::new();
    let mut err = None;
    for vout in tx.output.iter() {
        vouts.push(Vout {
            value: vout.value.to_sat(),
            script_pub_key: vout.script_pubkey.to_bytes(),
            //is_segwit: vout.script_pubkey.is_witness_program(),
        });
    }
    let mut parsed_vouts: Result<ParsedVouts, String> = Err("Not Parsed".to_string());
    if parse_vouts {
        for (vi, vout) in tx.output.iter().enumerate() {
            if !config.multisig_addresses_enabled(height) {
                continue;
            }
            let output_value = vout.value.to_sat() as i64;
            fee -= output_value;
            let result = parse_vout(
                &config,
                key.clone(),
                height,
                tx.compute_txid().to_string(),
                vi,
                &vout.clone(),
            );
            match result {
                Err(e) => {
                    err = Some(e);
                    break;
                }
                Ok((parse_output, potential_dispenser)) => {
                    potential_dispensers.push(potential_dispenser);
                    if data.is_empty()
                        && parse_output.is_destination()
                        && destinations != vec![config.unspendable()]
                    {
                        if let ParseOutput::Destination(destination) = parse_output {
                            destinations.push(destination);
                        }
                        btc_amount += output_value;
                    } else if parse_output.is_destination() {
                        break;
                    } else if let ParseOutput::Data(mut new_data) = parse_output {
                        // reveal transaction data
                        if config.taproot_support_enabled(height) && new_data == b"CNTRPRTY" && !vtxinwit.is_empty() && vtxinwit[0].len() == 3 {
                            if let Ok(bytes) = hex::decode(&vtxinwit[0][1]) {
                                let script = Script::from_bytes(&bytes);
                                match extract_data_from_witness(&script) {
                                    Ok(mut inscription_data) => {
                                        if !inscription_data.is_empty() {
                                            is_reveal_tx = true;
                                            data.append(&mut inscription_data);
                                        }
                                    },
                                    Err(e) => {
                                        err = Some(Error::ParseVout(format!(
                                            "Failed to extract data from witness script: {} for tx: {}",
                                            e,
                                            tx.compute_txid().to_string()
                                        )));
                                    }
                                }
                            } else {
                                err = Some(Error::ParseVout(format!(
                                    "Failed to decode taproot witness hex for tx: {}",
                                    tx.compute_txid().to_string()
                                )));
                            }
                        } else {
                            data.append(&mut new_data)
                        }
                    }
                }
            }
        }
        if !config.multisig_addresses_enabled(height) {
            err = Some(Error::ParseVout(
                "Multisig addresses are not enabled".to_string(),
            ));
        }
        parsed_vouts = if let Some(e) = err {
            Err(e.to_string())
        } else {
            Ok(ParsedVouts {
                destinations,
                btc_amount,
                fee,
                data: data.clone(),
                potential_dispensers,
                is_reveal_tx,
            })
        };
    }

    // Try to get previous transactions info if RPC is available and data is not empty
    let mut prev_txs = vec![None; tx.input.len()];
    if !data.is_empty() || 
        parsed_vouts.as_ref().map_or(false, |p| p.destinations == vec![config.unspendable()]) {

        if BATCH_CLIENT.lock().unwrap().is_none() {
            *BATCH_CLIENT.lock().unwrap() = Some(
                BatchRpcClient::new(
                    config.rpc_address.clone(),
                    config.rpc_user.clone(),
                    config.rpc_password.clone(),
                )
                .unwrap(),
            );
        }

        if let Some(batch_client) = BATCH_CLIENT.lock().unwrap().as_ref() {

            let input_txids: Vec<_> = tx
                .input
                .iter()
                .map(|vin| vin.previous_output.txid)
                .collect();
            prev_txs = batch_client
                .get_transactions(&input_txids)
                .unwrap_or_default();

            if is_reveal_tx && !prev_txs.is_empty() {
                if let Some(prev_tx) = &prev_txs[0] {
                    if !prev_tx.input.is_empty() {
                        commit_parent_txid = prev_tx.input[0].previous_output.txid;
                        commit_parent_vout = prev_tx.input[0].previous_output.vout as usize;
                        if let Ok(fetched_txs) = batch_client.get_transactions(&[commit_parent_txid]) {
                            if !fetched_txs.is_empty() {
                                prev_txs[0] = fetched_txs[0].clone();
                            }
                        }
                    }
                }
            }
        }
    }

    for (i, vin) in tx.input.iter().enumerate() {
        let hash = vin.previous_output.txid.to_string();
        let vin_info = prev_txs.get(i).and_then(|prev_tx| {
            prev_tx.as_ref().and_then(|tx| {
                let tx_id = tx.compute_txid();
                let vout_idx = if tx_id == commit_parent_txid {
                    commit_parent_vout
                } else {
                    vin.previous_output.vout as usize
                };

                let is_segwit = tx_id.to_string() != tx.compute_wtxid().to_string();

                tx.output.get(vout_idx).map(|output| VinOutput {
                    value: output.value.to_sat(),
                    script_pub_key: output.script_pubkey.to_bytes(),
                    is_segwit: if config.fix_is_segwit_enabled(height) { 
                        output.script_pubkey.is_witness_program()
                    } else {
                        is_segwit
                    },
                })
            })
        });

        vins.push(Vin {
            hash,
            n: vin.previous_output.vout,
            sequence: vin.sequence.0,
            script_sig: vin.script_sig.to_bytes(),
            info: vin_info,
        });
    }

    let tx_id = tx.compute_txid().to_string();
    let tx_hash;
    if segwit && config.correct_segwit_txids_enabled(height) {
        tx_hash = tx_id.clone();
    } else {
        tx_hash = Sha256dHash::hash(&tx_bytes).to_string();
    }

    Transaction {
        version: tx.version.0,
        segwit,
        coinbase: tx.is_coinbase(),
        lock_time: tx.lock_time.to_consensus_u32(),
        tx_id,
        tx_hash,
        vtxinwit,
        vin: vins,
        vout: vouts,
        parsed_vouts,
    }
}

impl ToBlock for Block {
    fn to_block(&self, config: Config, height: u32) -> CrateBlock {
        let mut transactions = Vec::new();
        for tx in self.txdata.iter() {
            transactions.push(parse_transaction(tx, &config, height, true));
        }
        CrateBlock {
            height,
            version: self.header.version.to_consensus(),
            hash_prev: self.header.prev_blockhash.to_string(),
            hash_merkle_root: self.header.merkle_root.to_string(),
            block_time: self.header.time,
            bits: self.header.bits.to_consensus(),
            nonce: self.header.nonce,
            block_hash: self.block_hash().to_string(),
            transaction_count: self.txdata.len(),
            transactions,
        }
    }
}

pub fn parse_block(
    block: Block,
    config: &Config,
    height: u32,
    parse_vouts: bool,
) -> Result<CrateBlock, Error> {
    let mut transactions = Vec::new();
    for tx in block.txdata.iter() {
        transactions.push(parse_transaction(tx, config, height, parse_vouts));
    }
    Ok(CrateBlock {
        height,
        version: block.header.version.to_consensus(),
        hash_prev: block.header.prev_blockhash.to_string(),
        hash_merkle_root: block.header.merkle_root.to_string(),
        block_time: block.header.time,
        bits: block.header.bits.to_consensus(),
        nonce: block.header.nonce,
        block_hash: block.block_hash().to_string(),
        transaction_count: block.txdata.len(),
        transactions,
    })
}

impl BlockHasPrevBlockHash for Block {
    fn get_prev_block_hash(&self) -> &BlockHash {
        &self.header.prev_blockhash
    }
}

pub trait BitcoinRpc<B>: Send + Clone + 'static {
    fn get_block_hash(&self, height: u32) -> Result<BlockHash, Error>;
    fn get_block(&self, hash: &BlockHash) -> Result<Box<B>, Error>;
    fn get_blockchain_height(&self) -> Result<u32, Error>;
}

struct GetBlockHash {
    height: u32,
    sender: Sender<Result<BlockHash, Error>>,
}

struct GetBlock {
    hash: BlockHash,
    sender: Sender<Result<Box<Block>, Error>>,
}

struct GetBlockchainHeight {
    sender: Sender<Result<u32, Error>>,
}

type Channel<T> = (Sender<T>, Receiver<T>);

#[derive(Clone)]
struct Channels {
    get_block_hash: Channel<GetBlockHash>,
    get_block: Channel<GetBlock>,
    get_blockchain_height: Channel<GetBlockchainHeight>,
}

impl Channels {
    fn new(n: usize) -> Self {
        Channels {
            get_block_hash: bounded(n),
            get_block: bounded(n),
            get_blockchain_height: bounded(n),
        }
    }
}

#[derive(Clone)]
pub struct BitcoinClient {
    n: usize,
    config: Config,
    stopper: Stopper,
    channels: Channels,
}

impl BitcoinClient {
    pub fn new(config: &Config, stopper: Stopper, n: usize) -> Result<Self, Error> {
        let client = Self {
            n,
            config: config.clone(),
            stopper,
            channels: Channels::new(n),
        };
        Ok(client)
    }

    pub fn start(&self) -> Result<Vec<JoinHandle<Result<(), Error>>>, Error> {
        let (_tx, _rx) = unbounded();
        let client = BitcoinClientInner::new(&self.config)?;
        new_worker_pool(
            "BitcoinClient".into(),
            self.n,
            _rx,
            _tx,
            self.stopper.clone(),
            Self::worker(client, self.channels.clone()),
        )
    }

    fn worker(
        client: BitcoinClientInner,
        channels: Channels,
    ) -> impl Fn(Receiver<()>, Sender<()>, Stopper) -> Result<(), Error> + Clone {
        move |_, _, stopper| loop {
            let (_, done) = stopper.subscribe()?;
            select! {
              recv(done) -> _ => {
                return Ok(())
              },
              recv(channels.get_block_hash.1) -> msg => {
                if let Ok(GetBlockHash {height, sender}) = msg {
                  sender.send(client.get_block_hash(height))?;
                }
              },
              recv(channels.get_block.1) -> msg => {
                if let Ok(GetBlock {hash, sender}) = msg {
                  sender.send(client.get_block(&hash))?;
                }
              },
              recv(channels.get_blockchain_height.1) -> msg => {
                if let Ok(GetBlockchainHeight {sender}) = msg {
                  sender.send(client.get_blockchain_height())?;
                }
              }
            }
        }
    }
}

impl BitcoinRpc<Block> for BitcoinClient {
    fn get_block_hash(&self, height: u32) -> Result<BlockHash, Error> {
        let (tx, rx) = bounded(1);
        self.channels
            .get_block_hash
            .0
            .send(GetBlockHash { height, sender: tx })?;
        let (id, done) = self.stopper.subscribe()?;
        select! {
            recv(done) -> _ => Err(Error::Stopped),
            recv(rx) -> result => {
                self.stopper.unsubscribe(id)?;
                result?
            }
        }
    }

    fn get_block(&self, hash: &BlockHash) -> Result<Box<Block>, Error> {
        let (tx, rx) = bounded(1);
        self.channels.get_block.0.send(GetBlock {
            hash: *hash,
            sender: tx,
        })?;
        let (id, done) = self.stopper.subscribe()?;
        select! {
            recv(done) -> _ => Err(Error::Stopped),
            recv(rx) -> result => {
                self.stopper.unsubscribe(id)?;
                result?
            }
        }
    }

    fn get_blockchain_height(&self) -> Result<u32, Error> {
        let (tx, rx) = bounded(1);
        self.channels
            .get_blockchain_height
            .0
            .send(GetBlockchainHeight { sender: tx })?;
        let (id, done) = self.stopper.subscribe()?;
        select! {
            recv(done) -> _ => Err(Error::Stopped),
            recv(rx) -> result => {
                self.stopper.unsubscribe(id)?;
                result?
            }
        }
    }
}

#[derive(Clone)]
struct BitcoinClientInner {
    client: Arc<BatchRpcClient>,
}

impl BitcoinClientInner {
    fn new(config: &Config) -> Result<Self, Error> {
        let client = BatchRpcClient::new(
            config.rpc_address.clone(),
            config.rpc_user.clone(),
            config.rpc_password.clone(),
        )
        .map_err(|e| Error::BitcoinRpc(format!("Failed to create BatchRpcClient: {:#?}", e)))?;

        Ok(BitcoinClientInner {
            client: Arc::new(client),
        })
    }
}

impl BitcoinRpc<Block> for BitcoinClientInner {
    fn get_block_hash(&self, height: u32) -> Result<BlockHash, Error> {
        self.client
            .get_block_hash(height)
            .map_err(|e| Error::BitcoinRpc(format!("Failed to get block hash: {:#?}", e)))
    }

    fn get_block(&self, hash: &BlockHash) -> Result<Box<Block>, Error> {
        self.client
            .get_block(hash)
            .map(Box::new)
            .map_err(|e| Error::BitcoinRpc(format!("Failed to get block: {:#?}", e)))
    }

    fn get_blockchain_height(&self) -> Result<u32, Error> {
        self.client
            .get_blockchain_info()
            .map_err(|e| Error::BitcoinRpc(format!("Failed to get blockchain info: {:#?}", e)))
            .and_then(|info| {
                info["blocks"]
                    .as_u64()
                    .ok_or_else(|| {
                        Error::BitcoinRpc("Invalid blocks field in blockchain info".into())
                    })
                    .map(|h| h as u32)
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use bitcoin::hashes::{sha256d, Hash};
    use bitcoin::{
        absolute::LockTime,
        block::{self, Header},
        transaction::Version,
        Amount, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxMerkleNode,
        TxOut, Txid, Witness,
    };

    use crate::indexer::{
        test_utils::{test_block_hash, test_h160_hash, test_sha256_hash},
        types::entry::FromEntry,
    };

    use super::*;

    #[test]
    fn test_get_entries() {
        let height = 2;

        let script_pubkey = ScriptBuf::from_bytes(test_h160_hash(0).to_vec());

        let tx_in = TxIn {
            previous_output: OutPoint {
                txid: Txid::from_raw_hash(sha256d::Hash::from_slice(&test_sha256_hash(0)).unwrap()),
                vout: 1,
            },
            script_sig: ScriptBuf::from_bytes(test_h160_hash(0).to_vec()),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        };

        let tx_out = TxOut {
            value: Amount::from_sat(1),
            script_pubkey: script_pubkey.clone(),
        };

        let tx = Transaction {
            version: Version::ONE,
            lock_time: LockTime::ZERO,
            input: vec![tx_in],
            output: vec![tx_out],
        };

        let block = Block {
            header: Header {
                version: block::Version::ONE,
                prev_blockhash: test_block_hash(1),
                merkle_root: TxMerkleNode::from_raw_hash(
                    sha256d::Hash::from_slice(&test_sha256_hash(height)).unwrap(),
                ),
                time: 1234567890,
                bits: CompactTarget::default(),
                nonce: 0,
            },
            txdata: vec![tx],
        };

        let entries = block.get_entries(Mode::Indexer, height);

        let entry = entries.first().unwrap().to_entry();
        let e = BlockAtHeightHasHash::from_entry(entry).unwrap();
        assert_eq!(e.height, height);
        assert_eq!(e.hash, block.block_hash().as_byte_array().to_owned());

        let entry = entries.get(1).unwrap().to_entry();
        let e = TxInBlockAtHeight::from_entry(entry).unwrap();
        assert_eq!(e.txid, block.txdata[0].compute_txid().to_byte_array());
        assert_eq!(e.height, height);

        let entry = entries.get(2).unwrap().to_entry();
        let e = BlockAtHeightSpentOutputInTx::from_entry(entry).unwrap();
        assert_eq!(e.txid, test_sha256_hash(0));
        assert_eq!(e.vout, 1);
        assert_eq!(e.height, height);

        let entry = entries.get(3).unwrap().to_entry();
        let e = ScriptHashHasOutputsInBlockAtHeight::from_entry(entry).unwrap();
        assert_eq!(
            e.script_hash.to_vec(),
            script_pubkey.script_hash().as_byte_array().to_vec()
        );
        assert_eq!(e.height, height);
    }
}
