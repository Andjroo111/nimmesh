# Binary SMS transport: one transaction in one SMS

The design for issue #7 — the cell-signal-but-no-data path. Research and design only;
no SMS code exists yet, and the standing honesty note applies: nothing below is
field-tested, and the carrier survey is explicitly open fieldwork.

## The correction this research forces

`TRANSPORTS.md` and the CI test `the_sms_headroom_is_exactly_one_byte_and_that_is_documented`
state that a 139-byte transaction fits a 140-byte binary SMS with one byte to spare and
"no User Data Header eating into the budget." That is true **at the raw PDU level** —
and misleading for the path an app can actually use.

For an Android app to *receive* a data SMS at all, the message must be **port-addressed**:
the receiver registers for `DATA_SMS_RECEIVED` with an `android:port`, and routing by
port requires a UDH in the message. The standard 16-bit application-port UDH
(`06 05 04 dddd ssss`) is **7 bytes**, and Android's send API (`SmsManager.sendDataMessage`,
which takes a 16-bit destination port) emits exactly that form. So the practical budget is:

```
140 − 7 (UDHL + application-port IE) = 133 bytes
```

**A 139-byte transaction does not fit one port-addressed data SMS.** Six bytes over.
The 8-bit-port variant of the IE (5-byte UDH, 135-byte budget) is still four bytes
short, and Android's API doesn't emit it anyway.

The PDU-level claim stays true and the CI test stays green — a modem reading raw PDUs
(a gateway, not a phone) can receive the unported 140-byte form. But phone-to-phone and
anything received *by the app* pays the port tax. `TRANSPORTS.md` is corrected in this
commit to say so.

## The design that fits anyway: the SMS compact form

The 139-byte basic wire carries three things a cooperating gateway can reconstruct
instead of transport: the fee (overwhelmingly 0 on Nimiq), the transaction type/flags
constants of a basic transfer, and nothing else — sender address is already derived
from the public key, not carried. So define a compact form for exactly the common case
(basic transfer, fee = 0):

```text
tag(1) | networkId(1) | senderPubKey(32) | recipient(20) | value(8)
      | validityStartHeight(4) | signature(64)                      = 130 bytes
```

130 ≤ 133: **one port-addressed data SMS, three bytes spare.** The receiving gateway
re-inflates the canonical 139-byte wire (fee = 0, basic type constants, the carried
fields verbatim) and hands it to the normal relay path. The signature was made over the
canonical serialization, so a correct re-inflation broadcasts unchanged — and an
incorrect one fails signature verification on chain, never silently pays the wrong
thing. The `tag` byte versions the form and doubles as the foreign-traffic filter on
the shared port.

What the compact form does **not** cover, stated plainly:

- **fee ≠ 0** — not SMS-eligible in v1 (fallback: two concatenated data SMS; the
  concatenation IE adds 5–6 more UDH bytes, leaving ~127 per segment, so 140 bytes of
  payload need exactly 2 segments).
- **memo transactions (~205 B)** — never SMS-eligible, same as `TRANSPORTS.md` already
  states.
- Everything else the mesh floods (receipts at 33 B, beacons at 37 B, balance
  responses at 33 B) fits a single port-addressed SMS trivially, prefixed with the same
  tag byte.

Proposed CI additions when the codec lands, next to the existing MTU tests:
`the_sms_compact_form_fits_the_port_addressed_budget` (130 ≤ 133, from a real encoder)
and a test that re-inflates a compact form and asserts byte-equality with
`serialize_basic` output — the compact form is only safe while that equality holds.

## Android mechanics

- **Send:** `SmsManager.sendDataMessage(dest, null, PORT, bytes, sentPI, deliveryPI)`.
  Long-standing API, present on current SDKs (the deprecated variant is only the old
  `android.telephony.gsm` package). `SEND_SMS` is a runtime permission.
- **Receive:** a manifest receiver for `android.intent.action.DATA_SMS_RECEIVED` with
  `scheme="sms"` and the chosen port; `RECEIVE_SMS` permission. Data SMS on a port does
  not require being the default SMS app, and the message never appears in the user's
  messaging thread.
- **Port:** one constant (16-bit, from the unassigned range, e.g. 46464), same value
  both directions, config-overridable like the LoRa portnum.
- **Distribution reality:** Google Play's SMS/Call-Log policy restricts `SEND_SMS` /
  `RECEIVE_SMS` to default-SMS handlers and narrow exceptions. NIMmesh ships as a
  sideloaded APK from its own site, so the policy does not bind today — but it makes
  a Play listing with this feature effectively impossible, which is worth knowing
  before anyone proposes one.
- **iOS: no.** There is no data-SMS receive API on iOS at all. SMS transport is
  Android-and-gateway only, permanently.

## Roles: who texts whom

SMS is a **gateway-of-last-resort egress**, not a mesh. Two shapes:

1. **Phone → phone-gateway.** A node with bars but no data texts the compact form to a
   volunteer gateway phone (Android, app installed, has data or will have it). The
   gateway app re-inflates and floods the tx into the normal path. This is the
   pure-app path: no infrastructure, no modem, works today's hardware.
2. **Phone → modem gateway.** A fixed number backed by a modem that reads raw PDUs
   (the always-on gateway host). Same compact form; the modem side has no 133-byte
   constraint but accepts the same format for one codec everywhere.

The receipt (`0x31`, 33 B + tag) rides back over SMS to the sender's number on
request — `wantReceipt` maps to "reply with the receipt" — closing the loop for a
sender who may stay data-dark.

## Cost and abuse, the actual constraints

- **SMS costs real money per message.** Sender-side: strictly **opt-in, per
  transaction** — a deliberate "send via SMS" action in the UI, never an automatic
  fallback and never automatic relay of *other people's* transactions (a mesh that
  auto-texted every relayed tx would drain strangers' prepaid balances; that path is
  designed out, not just discouraged).
- **Gateway-side abuse:** an SMS gateway number is a DoS target (each inbound message
  is free to the attacker on many plans, and each outbound receipt costs the gateway).
  Receipts-over-SMS are therefore rate-limited per sender number and total; inbound
  parsing is the same hostile-input codec discipline as every mesh decoder.
- **Privacy:** an SMS exposes the sender's phone number to the gateway and both
  numbers to carriers — strictly worse metadata than BLE flooding. The UI copy for the
  opt-in must say so in one sentence.

## Carrier survey: what is actually known

What the sources establish: 8-bit port-addressed data SMS is standard GSM 03.40/03.38
machinery, it is what WAP push and visual-voicemail notifications ride, and Android has
supported both directions since API 4. What they also establish: UDH handling across
**aggregator/A2P routes** is unreliable (stripped or mangled headers are a documented
failure class), and legacy CDMA paths had their own encoding quirks.

The honest survey is a test matrix, not a literature claim:

| leg | expectation | verdict |
| --- | --- | --- |
| same-carrier phone → phone (each major US carrier) | best case for UDH survival | untested |
| cross-carrier phone → phone | the common real case | untested |
| phone → modem gateway (direct SIM) | no aggregator in path | untested |
| A2P/aggregator number → phone | documented UDH risk; likely worst | untested |

Two prepaid SIMs and an afternoon fill in the first three rows; the fourth only
matters if a hosted number is ever used. Until the matrix has verdicts, no public copy
may claim SMS transport "works" — it is designed, with its budget proven by
arithmetic and its delivery unproven by anyone.

## Build order

1. **Compact-form codec in `nimmesh-core`** — encode/re-inflate + the two CI tests.
   Pure Rust, no telephony, testable now.
2. **Android send + receive** behind a config flag, off by default, opt-in UI.
3. **The carrier matrix** with two prepaid SIMs — the SMS twin of issue #3.
4. Modem-gateway ingest on the always-on host, if 1–3 prove out.
