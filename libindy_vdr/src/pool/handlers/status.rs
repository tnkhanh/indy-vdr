use std::cmp::Ordering;
use std::collections::HashSet;

use futures_util::stream::StreamExt;

use crate::common::error::prelude::*;
use crate::common::merkle_tree::MerkleTree;
use crate::utils::base58;

use super::types::Message;
use super::{
    check_cons_proofs, min_consensus, ConsensusState, PoolRequest, ReplyState, RequestEvent,
    RequestResult, RequestResultMeta,
};

pub type CatchupTarget = (Vec<u8>, usize, Vec<String>);

pub async fn handle_status_request<R: PoolRequest>(
    request: &mut R,
    merkle_tree: MerkleTree,
) -> VdrResult<(RequestResult<Option<CatchupTarget>>, RequestResultMeta)> {
    trace!("status request");
    let config = request.pool_config();
    let total_node_count = request.node_count();
    let mut replies = ReplyState::new();
    let mut consensus = ConsensusState::new();
    let f = min_consensus(total_node_count);
    request.send_to_all(config.reply_timeout)?;
    loop {
        match request.next().await {
            Some(RequestEvent::Received(node_alias, raw_msg, parsed)) => {
                match parsed {
                    Message::LedgerStatus(ls) => {
                        trace!("Received ledger status from {}", &node_alias);
                        replies.add_reply(node_alias.clone(), true);
                        let key = (ls.merkleRoot.clone(), ls.txnSeqNo, None);
                        consensus.insert(key, node_alias.clone());
                    }
                    Message::ConsistencyProof(cp) => {
                        trace!("Received consistency proof from {}", &node_alias);
                        replies.add_reply(node_alias.clone(), true);
                        let key = (cp.newMerkleRoot.clone(), cp.seqNoEnd, Some(cp.hashes));
                        consensus.insert(key, node_alias.clone());
                    }
                    Message::ReqACK(_) => continue,
                    Message::ReqNACK(_) | Message::Reject(_) => {
                        debug!("Status request failed for {}", &node_alias);
                        replies.add_failed(node_alias.clone(), raw_msg);
                    }
                    _ => {
                        debug!("Unexpected reply from {}", &node_alias);
                        replies.add_failed(node_alias.clone(), raw_msg);
                    }
                };
                request.clean_timeout(node_alias)?;
            }
            Some(RequestEvent::Timeout(node_alias)) => {
                replies.add_timeout(node_alias);
            }
            None => {
                return Ok((
                    RequestResult::Failed(err_msg(
                        VdrErrorKind::PoolTimeout,
                        "Request was interrupted",
                    )),
                    request.get_meta(),
                ))
            }
        };
        match check_nodes_responses_on_status(
            &merkle_tree,
            &replies,
            &consensus,
            total_node_count,
            f,
        ) {
            Ok(CatchupProgress::NotNeeded) => {
                return Ok((RequestResult::Reply(None), request.get_meta()));
            }
            Ok(CatchupProgress::InProgress) => {}
            Ok(CatchupProgress::NoConsensus) => {
                return Ok((
                    RequestResult::Failed(replies.get_error()),
                    request.get_meta(),
                ));
            }
            Ok(CatchupProgress::ShouldBeStarted(target)) => {
                return Ok((RequestResult::Reply(Some(target)), request.get_meta()));
            }
            Err(err) => return Ok((RequestResult::Failed(err), request.get_meta())),
        };
    }
}

enum CatchupProgress {
    ShouldBeStarted(CatchupTarget),
    NoConsensus,
    NotNeeded,
    InProgress,
}

fn check_nodes_responses_on_status<R>(
    merkle_tree: &MerkleTree,
    replies: &ReplyState<R>,
    consensus: &ConsensusState<(String, usize, Option<Vec<String>>), String>,
    total_nodes_count: usize,
    f: usize,
) -> VdrResult<CatchupProgress> {
    let max_consensus = if let Some((most_popular_vote, votes)) = consensus.max_entry() {
        let votes_count = votes.len();
        if votes_count > f {
            return try_to_catch_up(most_popular_vote, merkle_tree, votes);
        }
        votes_count
    } else {
        0
    };
    if max_consensus + total_nodes_count - replies.len() <= f {
        return Ok(CatchupProgress::NoConsensus);
    }
    Ok(CatchupProgress::InProgress)
}

fn try_to_catch_up(
    ledger_status: &(String, usize, Option<Vec<String>>),
    merkle_tree: &MerkleTree,
    nodes: &HashSet<String>,
) -> VdrResult<CatchupProgress> {
    let &(ref target_mt_root, target_mt_size, ref hashes) = ledger_status;
    let cur_mt_size = merkle_tree.count();
    let cur_mt_hash = base58::encode(merkle_tree.root_hash());

    match target_mt_size.cmp(&cur_mt_size) {
        Ordering::Equal => {
            if cur_mt_hash.eq(target_mt_root) {
                Ok(CatchupProgress::NotNeeded)
            } else {
                Err(input_err(
                    "Ledger merkle tree is not acceptable for current tree.",
                ))
            }
        }
        Ordering::Greater => {
            let target_mt_root = base58::decode(target_mt_root)
                .with_input_err("Can't parse target MerkleTree hash from nodes responses")?;

            match *hashes {
                None => (),
                Some(ref hashes) => {
                    check_cons_proofs(merkle_tree, hashes, &target_mt_root, target_mt_size)?
                }
            };

            Ok(CatchupProgress::ShouldBeStarted((
                target_mt_root,
                target_mt_size,
                nodes.iter().cloned().collect(),
            )))
        }
        _ => Err(input_err("Local merkle tree greater than mt from ledger")),
    }
}
