//! Pure logic of the Safe transaction simulator: no I/O, so it can compile to
//! WASM as-is. The server, HTTP calls and the `cast` subprocess live in main.rs.

use alloy::hex;
use alloy::primitives::{Signed, U256, U512};
use alloy::sol_types::SolCall;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const EXEC_SIG: &str = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,\
                            uint256,address,address,bytes)";
// ponytail: threshold at slot 4 is the Safe 1.x layout only; revisit for others.
pub const THRESHOLD_SLOT: &str = "0x4";
pub const THRESHOLD_ONE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";
pub const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

alloy::sol! {
    function execTransaction(address to, uint256 value, bytes data, uint8 operation,
        uint256 safeTxGas, uint256 baseGas, uint256 gasPrice, address gasToken,
        address refundReceiver, bytes signatures) returns (bool success);
}

/// Wide enough that summing U256 amounts never overflows.
pub type I512 = Signed<512, 8>;
/// (token, from, to, amount); token is "ETH" for native value.
pub type Move = (String, String, String, U256);

/// JSON scalar as text, like Python's `str()`: strings unquoted, numbers as-is.
pub fn value_str(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
}

pub fn is_address(a: &str) -> bool {
    a.len() == 42 && a.starts_with("0x") && a[2..].bytes().all(|c| c.is_ascii_hexdigit())
}

/// Validate a list of Safe addresses: lowercased, deduplicated, sorted.
pub fn validate_safes(safes: &[Value]) -> Result<Vec<String>, String> {
    let mut clean = std::collections::BTreeSet::new();
    for a in safes {
        let a = value_str(a).trim().to_lowercase();
        if !is_address(&a) {
            return Err(format!("Invalid address: {a:?}"));
        }
        clean.insert(a);
    }
    Ok(clean.into_iter().collect())
}

/// Validate a {"0xaddr": "label"} book; keys lowercased, labels trimmed.
pub fn validate_book(book: &Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
    let mut clean = BTreeMap::new();
    for (a, label) in book {
        let label = value_str(label).trim().to_string();
        if !is_address(a) {
            return Err(format!("Invalid address: {a:?}"));
        }
        if label.is_empty() || label.chars().count() > 64 || label.contains([':', '\n']) {
            return Err(format!("Label for {a} must be 1-64 chars, no ':' or newline."));
        }
        clean.insert(a.to_lowercase(), label);
    }
    Ok(clean)
}

/// Pre-validated "approved hash" signature for one owner: r=owner, s=0, v=1.
///
/// Valid only when msg.sender == owner (we pass --from owner), with threshold
/// overridden to 1. 65 bytes = 32 (r) + 32 (s) + 1 (v).
pub fn build_signature(owner: &str) -> String {
    let owner = owner.to_lowercase().replace("0x", "");
    format!("0x{owner:0>64}{}01", "0".repeat(64))
}

/// ABI-encoded execTransaction call from the same 10 string args given to cast.
pub fn exec_calldata(a: &[String]) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let call = execTransactionCall {
        to: a[0].parse()?,
        value: a[1].parse()?,
        data: a[2].parse()?,
        operation: a[3].parse()?,
        safeTxGas: a[4].parse()?,
        baseGas: a[5].parse()?,
        gasPrice: a[6].parse()?,
        gasToken: a[7].parse()?,
        refundReceiver: a[8].parse()?,
        signatures: a[9].parse()?,
    };
    Ok(hex::encode_prefixed(call.abi_encode()))
}

/// Pre-order walk of a callTracer tree, skipping reverted frames (their
/// effects, and every sub-call's, are rolled back).
fn walk(f: &Value, out: &mut Vec<Move>) {
    if f["error"].as_str().is_some_and(|e| !e.is_empty()) {
        return;
    }
    let value: U256 = f["value"].as_str().filter(|v| !v.is_empty()).unwrap_or("0x0").parse().unwrap_or_default();
    if !value.is_zero() && !matches!(f["type"].as_str(), Some("DELEGATECALL" | "STATICCALL")) {
        out.push(("ETH".into(), value_str(&f["from"]), value_str(&f["to"]), value));
    }
    for log in f["logs"].as_array().into_iter().flatten() {
        let t: Vec<&str> = log["topics"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let data = log["data"].as_str().unwrap_or_default();
        if t.len() == 3 && t[0] == TRANSFER_TOPIC && data.len() >= 66 {
            let addr = |topic: &str| format!("0x{}", &topic[topic.len().saturating_sub(40)..]);
            let amount = U256::from_str_radix(&data[2..66], 16).unwrap_or_default();
            out.push((value_str(&log["address"]), addr(t[1]), addr(t[2]), amount));
        }
    }
    for c in f["calls"].as_array().into_iter().flatten() {
        walk(c, out);
    }
}

/// Every ERC20 Transfer and ETH value move. ERC721 Transfer (4 topics) is ignored.
pub fn extract_movements(root: &Value) -> Vec<Move> {
    let mut moves = Vec::new();
    walk(root, &mut moves);
    moves
}

pub fn to_i512(n: U256) -> I512 {
    I512::from_raw(U512::from(n))
}

/// {(token, holder): net delta}, zero deltas dropped.
pub fn net_balances(moves: &[Move]) -> BTreeMap<(String, String), I512> {
    let mut bal: BTreeMap<(String, String), I512> = BTreeMap::new();
    for (token, src, dst, amt) in moves {
        *bal.entry((token.clone(), src.clone())).or_default() -= to_i512(*amt);
        *bal.entry((token.clone(), dst.clone())).or_default() += to_i512(*amt);
    }
    bal.retain(|_, v| !v.is_zero());
    bal
}

/// ABI string, or bytes32 for old tokens like MKR.
pub fn decode_string(hexdata: &str) -> String {
    let b = hex::decode(hexdata).unwrap_or_default();
    // Python-style clamped slice: out-of-range offsets give an empty slice.
    let sl = |s: usize, e: usize| &b[s.min(b.len())..e.min(b.len()).max(s.min(b.len()))];
    if b.len() == 32 {
        let end = b.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        return String::from_utf8_lossy(&b[..end]).into_owned();
    }
    if b.len() >= 64 {
        let word = |s: usize| usize::try_from(U256::from_be_slice(sl(s, s.saturating_add(32)))).unwrap_or(usize::MAX);
        let off = word(0);
        let n = word(off);
        let start = off.saturating_add(32);
        return String::from_utf8_lossy(sl(start, start.saturating_add(n))).into_owned();
    }
    String::new()
}

pub fn fmt_amount(n: I512, decimals: u8) -> String {
    let scale = U512::from(10).pow(U512::from(decimals));
    let (q, r) = n.unsigned_abs().div_rem(scale);
    let digits = q.to_string();
    let mut int = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            int.push(',');
        }
        int.push(c);
    }
    let frac = if r.is_zero() {
        String::new()
    } else {
        format!(".{:0>w$}", r.to_string(), w = decimals as usize).trim_end_matches('0').to_string()
    };
    format!("{}{int}{frac}", if n.is_negative() { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn address_book_validation() {
        let ok = json!({"0xABCDEF0000000000000000000000000000000002": " Ops "});
        let clean = validate_book(ok.as_object().unwrap()).unwrap();
        assert_eq!(clean, BTreeMap::from([("0xabcdef0000000000000000000000000000000002".into(), "Ops".into())]));
        let a = format!("0x{}", "a".repeat(40));
        for bad in [json!({"0x12": "x"}), json!({&a: ""}), json!({&a: "a:b"})] {
            assert!(validate_book(bad.as_object().unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn safes_validation() {
        let a = format!("0x{}", "A".repeat(40));
        let ok = validate_safes(&[json!(a), json!(a.to_lowercase())]).unwrap();
        assert_eq!(ok, [a.to_lowercase()]);
        assert!(validate_safes(&[json!("0x12")]).is_err());
    }

    #[test]
    fn signature() {
        let sig = build_signature("0xdC0046B52e2E38eEb2D007e0b3e3db7C37C50c80");
        assert_eq!(sig.len(), 2 + 130);
        assert!(sig.ends_with("01"));
        assert!(sig[2..66].ends_with("dc0046b52e2e38eeb2d007e0b3e3db7c37c50c80"));
        assert!(sig[2..66].starts_with("0000")); // address right-aligned
        assert_eq!(sig[66..130], "0".repeat(64)); // s == 0
    }

    #[test]
    fn calldata() {
        let z = "0x0000000000000000000000000000000000000000".to_string();
        let args = [&z, "0", "0x", "0", "0", "0", "0", &z, &z, "0x"].map(String::from);
        assert!(exec_calldata(&args).unwrap().starts_with("0x6a761202"));
    }

    #[test]
    fn movements_and_balances() {
        let pad = |a: &str| format!("0x{:0>64}", &a[2..]);
        let (a, b, c, tok) = ["a", "b", "c", "1"].map(|h| format!("0x{}", h.repeat(40))).into();
        let xfer = |s: &str, d: &str, n: u64| json!({"address": tok, "topics": [TRANSFER_TOPIC, pad(s), pad(d)],
                                                     "data": format!("0x{n:064x}")});
        let tree = json!({"type": "CALL", "from": a, "to": b, "value": "0x5", "logs": [xfer(&a, &b, 100)],
            "calls": [{"type": "CALL", "from": b, "to": c, "value": "0x0", "logs": [xfer(&b, &c, 30)]},
                      {"type": "CALL", "from": b, "to": c, "value": "0x9", "error": "revert",
                       "logs": [xfer(&b, &c, 999)]},
                      {"type": "DELEGATECALL", "from": b, "to": c, "value": "0x5"}]});
        let moves = extract_movements(&tree);
        let m = |t: &str, s: &str, d: &str, n: u64| (t.to_string(), s.to_string(), d.to_string(), U256::from(n));
        assert_eq!(moves, [m("ETH", &a, &b, 5), m(&tok, &a, &b, 100), m(&tok, &b, &c, 30)]);
        let i = |n: i64| I512::try_from(n).unwrap();
        let k = |t: &str, h: &str| (t.to_string(), h.to_string());
        assert_eq!(net_balances(&moves), BTreeMap::from([
            (k("ETH", &a), i(-5)), (k("ETH", &b), i(5)), (k(&tok, &a), i(-100)),
            (k(&tok, &b), i(70)), (k(&tok, &c), i(30))]));
    }

    #[test]
    fn formatting_and_strings() {
        let i = |n: i128| I512::try_from(n).unwrap();
        assert_eq!(fmt_amount(i(-1234567890), 6), "-1,234.56789");
        assert_eq!(fmt_amount(i(5 * 10i128.pow(18)), 18), "5");
        assert_eq!(fmt_amount(i(7), 0), "7");
        assert_eq!(decode_string(&format!("0x{:0<64}", hex::encode("MKR"))), "MKR");
        let abi = format!("0x{:064x}{:064x}{:0<64}", 32, 4, hex::encode("USDC"));
        assert_eq!(decode_string(&abi), "USDC");
    }
}
