//! # chat — public mesh chat (`0x50`): the messenger layer of the mesh
//!
//! Bitchat-style **public broadcast chat** over the same flood/relay machinery every other
//! packet rides: dedup by blind relay key, TTL hop cap, store-and-forward (a peer that walks
//! into range later catches up via gossip-sync), degree-adaptive relay. A message is a small
//! self-contained payload — sender's wall-clock, a nickname, UTF-8 text — flooded to everyone.
//!
//! **Public means public**: every node on the mesh can read a `0x50`. The UI says so. The 1:1
//! **encrypted** lane already exists in the wire (`MessageType::NoiseEncrypted` `0x11` over
//! [`crate::noise`]) and is the follow-up, not this module.
//!
//! **Non-money-path.** Text only; no keys, no signing, no broadcast to any chain. The value of
//! chat here is the handshake layer for commerce: "meet me at the gate", a swap invite, a
//! cashlink — strings that make the money features usable between strangers.

use std::collections::VecDeque;

use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::node::MeshNode;
use crate::packet::{MessageType, Packet, PEER_ID_LEN};

/// Chat payload codec version.
pub const CHAT_VERSION: u8 = 1;
/// Longest nickname carried on the wire, in UTF-8 bytes.
pub const MAX_NICK_BYTES: usize = 32;
/// Longest message text carried on the wire, in UTF-8 bytes (single-frame budget —
/// the SMS discipline; longer messages are refused at send, never truncated silently).
pub const MAX_TEXT_BYTES: usize = 160;
/// Rolling log size a node keeps (per node, oldest evicted first).
pub const CHAT_LOG_CAP: usize = 200;

/// One heard-or-sent chat message in the node's rolling log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEntry {
    /// The originator's 8-byte mesh sender id.
    pub sender: [u8; PEER_ID_LEN],
    /// The originator's per-packet sequence (packet `timestamp_ms`) — with `sender`,
    /// the message's identity for dedup.
    pub seq: u64,
    /// Display name the sender chose (≤ [`MAX_NICK_BYTES`]).
    pub nickname: String,
    /// The message text (≤ [`MAX_TEXT_BYTES`]).
    pub text: String,
    /// The sender's wall-clock at send, Unix ms (display only — never a protocol input).
    pub timestamp_ms: u64,
    /// Whether this node originated it.
    pub mine: bool,
}

/// The rolling public-chat log (newest last), deduped by `(sender, seq)`.
#[derive(Debug, Default)]
pub struct ChatLog {
    entries: VecDeque<ChatEntry>,
}

impl ChatLog {
    /// Append if unseen (linear scan — the log is small by design); evict oldest past cap.
    pub fn push(&mut self, entry: ChatEntry) {
        if self
            .entries
            .iter()
            .any(|e| e.sender == entry.sender && e.seq == entry.seq)
        {
            return;
        }
        self.entries.push_back(entry);
        while self.entries.len() > CHAT_LOG_CAP {
            self.entries.pop_front();
        }
    }

    /// The log, oldest → newest.
    pub fn entries(&self) -> impl Iterator<Item = &ChatEntry> {
        self.entries.iter()
    }
}

/// Encode a chat payload: `version(1) ‖ timestamp_ms(8 BE) ‖ nickLen(1) ‖ nick ‖ textLen(2 BE) ‖ text`.
/// `None` when a field is over budget or the text is empty (refuse, never truncate).
pub fn encode_chat(nickname: &str, text: &str, timestamp_ms: u64) -> Option<Vec<u8>> {
    let nick = nickname.as_bytes();
    let body = text.as_bytes();
    if nick.len() > MAX_NICK_BYTES || body.is_empty() || body.len() > MAX_TEXT_BYTES {
        return None;
    }
    let mut out = Vec::with_capacity(12 + nick.len() + body.len());
    out.push(CHAT_VERSION);
    out.extend_from_slice(&timestamp_ms.to_be_bytes());
    out.push(nick.len() as u8);
    out.extend_from_slice(nick);
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    Some(out)
}

/// Decode a chat payload. `None` on any malformed input — wrong version, truncation,
/// over-budget fields, invalid UTF-8, trailing bytes (mesh input is hostile).
pub fn decode_chat(payload: &[u8]) -> Option<(u64, String, String)> {
    let mut i = 0usize;
    let take = |i: &mut usize, n: usize| -> Option<&[u8]> {
        let s = payload.get(*i..*i + n)?;
        *i += n;
        Some(s)
    };
    if *take(&mut i, 1)?.first()? != CHAT_VERSION {
        return None;
    }
    let ts = u64::from_be_bytes(take(&mut i, 8)?.try_into().unwrap());
    let nick_len = *take(&mut i, 1)?.first()? as usize;
    if nick_len > MAX_NICK_BYTES {
        return None;
    }
    let nick = std::str::from_utf8(take(&mut i, nick_len)?)
        .ok()?
        .to_owned();
    let text_len = u16::from_be_bytes(take(&mut i, 2)?.try_into().unwrap()) as usize;
    if text_len == 0 || text_len > MAX_TEXT_BYTES {
        return None;
    }
    let text = std::str::from_utf8(take(&mut i, text_len)?)
        .ok()?
        .to_owned();
    if i != payload.len() {
        return None;
    }
    Some((ts, nick, text))
}

/// Flood a locally-authored chat message (worker thread): remember our own packet (echo
/// dedup + gossip-sync catch-up for late joiners), log it as `mine`, and flood.
pub(crate) fn flood_local_chat(ctx: &WorkerCtx, payload: Vec<u8>, st: &mut WorkerState) {
    let Some((ts, nick, text)) = decode_chat(&payload) else {
        return; // the FFI validated already; a malformed job is dropped, never flooded
    };
    let packet = ctx.build_packet(MessageType::Chat, payload);
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    ctx.chat.lock().unwrap().push(ChatEntry {
        sender: packet.sender_id,
        seq: packet.timestamp_ms,
        nickname: nick,
        text,
        timestamp_ms: ts,
        mine: true,
    });
    if let Ok(bytes) = crate::codec::encode(&packet) {
        ctx.flood(bytes);
    }
}

/// An inbound `0x50`: dedup, store-and-forward remember, log for the UI, blind-relay onward.
/// A malformed payload still relays (we never censor what we can't parse) but is not logged.
pub(crate) fn handle_chat_packet(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    remember(ctx, st, &packet);
    if let Some((ts, nick, text)) = decode_chat(&packet.payload) {
        ctx.chat.lock().unwrap().push(ChatEntry {
            sender: packet.sender_id,
            seq: packet.timestamp_ms,
            nickname: nick,
            text,
            timestamp_ms: ts,
            mine: packet.sender_id == ctx.sender_id,
        });
    }
    relay_onward(ctx, packet, src, st);
}

// --- FFI surface (a second `#[uniffi::export]` block on MeshNode, the tx_history pattern) ---

/// One chat message across FFI, shaped for the app's list.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiChatMessage {
    /// Stable message id (`hex(sender)-seq`).
    pub id: String,
    /// The sender's chosen display name.
    pub nickname: String,
    /// The message text.
    pub text: String,
    /// The sender's wall-clock at send, Unix ms.
    pub timestamp_ms: u64,
    /// Whether this node sent it.
    pub mine: bool,
}

#[uniffi::export]
impl MeshNode {
    /// Send a public chat message to the whole mesh (non-blocking enqueue; the worker
    /// floods it). `timestamp_ms` is the caller's wall-clock (display only). Returns
    /// `false` when the message is over the single-frame budget (`nickname` > 32 bytes,
    /// `text` empty or > 160 bytes) — refused, never truncated.
    pub fn send_chat(&self, nickname: String, text: String, timestamp_ms: u64) -> bool {
        match encode_chat(&nickname, &text, timestamp_ms) {
            Some(payload) => {
                self.enqueue_chat(payload);
                true
            }
            None => false,
        }
    }

    /// The rolling public-chat log this node has heard or sent, oldest → newest.
    pub fn chat_messages(&self) -> Vec<FfiChatMessage> {
        let log = self.ctx.chat.lock().unwrap();
        log.entries()
            .map(|e| FfiChatMessage {
                id: format!("{}-{}", crate::nimiq::hex::bytes_to_hex(&e.sender), e.seq),
                nickname: e.nickname.clone(),
                text: e.text.clone(),
                timestamp_ms: e.timestamp_ms,
                mine: e.mine,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_codec_roundtrips_and_rejects_hostile_input() {
        let p = encode_chat("andjroo", "meet me at the gate", 1_720_000_000_000).unwrap();
        let (ts, nick, text) = decode_chat(&p).unwrap();
        assert_eq!(
            (ts, nick.as_str(), text.as_str()),
            (1_720_000_000_000, "andjroo", "meet me at the gate")
        );

        // Refused at encode: empty text, oversize text, oversize nick.
        assert!(encode_chat("a", "", 1).is_none());
        assert!(encode_chat("a", &"x".repeat(MAX_TEXT_BYTES + 1), 1).is_none());
        assert!(encode_chat(&"n".repeat(MAX_NICK_BYTES + 1), "hi", 1).is_none());

        // Rejected at decode: wrong version, truncation, trailing bytes, bad UTF-8.
        let mut bad = p.clone();
        bad[0] = 9;
        assert!(decode_chat(&bad).is_none());
        assert!(decode_chat(&p[..p.len() - 1]).is_none());
        let mut trailing = p.clone();
        trailing.push(0);
        assert!(decode_chat(&trailing).is_none());
        let mut nonutf8 = encode_chat("a", "hi", 1).unwrap();
        let n = nonutf8.len();
        nonutf8[n - 1] = 0xFF;
        assert!(decode_chat(&nonutf8).is_none());
    }

    #[test]
    fn chat_log_dedupes_and_caps() {
        let mut log = ChatLog::default();
        let entry = |seq: u64| ChatEntry {
            sender: [1; PEER_ID_LEN],
            seq,
            nickname: "n".into(),
            text: "t".into(),
            timestamp_ms: seq,
            mine: false,
        };
        log.push(entry(7));
        log.push(entry(7)); // duplicate (a gossip-sync re-delivery) — dropped
        assert_eq!(log.entries().count(), 1);
        for s in 0..(CHAT_LOG_CAP as u64 + 10) {
            log.push(entry(100 + s));
        }
        assert_eq!(log.entries().count(), CHAT_LOG_CAP);
    }

    #[test]
    fn a_chat_message_crosses_the_mesh_and_lands_in_both_logs() {
        use crate::mock_radio::MeshHarness;

        let mut h = MeshHarness::new();
        let a = h.add_node("a", &[1]);
        let b = h.add_node("b", &[2]);
        h.connect("a", "b");

        assert!(a.send_chat("andjroo".into(), "gm mesh".into(), 42));
        // Deterministic drain: our jobs → the ether's delivery → B's handler (ADR-0005).
        a.fence();
        h.ether().fence();
        b.fence();

        let mine = a.chat_messages();
        assert_eq!(mine.len(), 1);
        assert!(mine[0].mine);
        let theirs = b.chat_messages();
        assert_eq!(theirs.len(), 1, "B must hear A's chat");
        assert_eq!(theirs[0].text, "gm mesh");
        assert_eq!(theirs[0].nickname, "andjroo");
        assert!(!theirs[0].mine);
        assert_eq!(theirs[0].id, mine[0].id, "same message, same identity");

        // Over-budget input is refused at the FFI edge.
        assert!(!a.send_chat("andjroo".into(), "x".repeat(200), 43));

        h.shutdown();
    }

    #[test]
    fn a_chat_missed_while_the_link_was_down_is_recovered_on_the_heartbeat() {
        use crate::mock_radio::MeshHarness;

        let mut h = MeshHarness::new();
        let a = h.add_node("a", &[1]);
        let b = h.add_node("b", &[2]);

        // A speaks while B is NOT linked — the flood reaches nobody.
        assert!(a.send_chat("andjroo".into(), "sent into the void".into(), 1));
        a.fence();
        h.ether().fence();
        b.fence();
        assert!(b.chat_messages().is_empty(), "no link, no delivery");

        // The link heals (the BLE flap ends) and B's heartbeat fires: gossip-sync asks
        // for what it missed and A serves it back. Andjroo's field bug (2026-07-08):
        // chats sent during a flap never arrived, because no shim ever ran the sync
        // tick — it now rides BeaconTick, the one heartbeat real devices actually have.
        h.connect("a", "b");
        b.poll_beacon();
        for _ in 0..3 {
            b.fence();
            h.ether().fence();
            a.fence();
            h.ether().fence();
        }
        let got = b.chat_messages();
        assert_eq!(got.len(), 1, "the heartbeat must recover the missed chat");
        assert_eq!(got[0].text, "sent into the void");
        h.shutdown();
    }
}
