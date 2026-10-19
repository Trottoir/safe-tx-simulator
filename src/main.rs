//! Self-hosted Safe transaction simulator.
//!
//! Fetches a pending Safe (Gnosis Safe) multisig tx from the Safe Transaction
//! Service by safeTxHash, then simulates it against the latest mainnet block with
//! a single `cast call --trace` (overriding the Safe threshold to 1 so no real
//! signatures are needed), and shows the full decoded trace in the browser.
//!
//!     export MAINNET_RPC_URL=https://...
//!     cargo run --release    # serve on http://localhost:8000
//!     cargo test             # run self-checks

use alloy::hex;
use alloy::primitives::Address;
use alloy::providers::{CallItem, Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use reqwest::Client;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tx_simulator::*;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const TX_SERVICE: &str = "https://api.safe.global/tx-service/eth/api/v1";
static BOOK_LOCK: Mutex<()> = Mutex::new(());

fn here(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(p)
}

/// {"0xaddr": "label"} from addressbook.json, keys lowercased. Missing file -> {}.
/// Re-read on every simulation, so edits apply without a restart.
fn load_address_book() -> Res<BTreeMap<String, String>> {
    let path = here("addressbook.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let book: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    Ok(book.iter().map(|(a, l)| (a.to_lowercase(), value_str(l))).collect())
}

/// Validate and write the whole book atomically (tmp file + rename).
fn save_address_book(book: &Map<String, Value>) -> Res<BTreeMap<String, String>> {
    let clean = validate_book(book)?;
    let _guard = BOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, tmp) = (here("addressbook.json"), here("addressbook.json.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&clean)?)?;
    std::fs::rename(tmp, path)?;
    Ok(clean)
}

/// Saved Safe addresses from safes.json. Missing file -> [].
fn load_safes() -> Res<Vec<String>> {
    let path = here("safes.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn save_safes(safes: &[Value]) -> Res<Vec<String>> {
    let clean = validate_safes(safes)?;
    let _guard = BOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, tmp) = (here("safes.json"), here("safes.json.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&clean)?)?;
    std::fs::rename(tmp, path)?;
    Ok(clean)
}

/// Queued txs of one Safe: not executed and nonce >= the Safe's current nonce
/// (`executed=false` alone also returns stale, replaced txs).
async fn pending_for(c: &Client, safe: &str) -> Res<Vec<Value>> {
    let addr = safe.parse::<Address>()?.to_checksum(None); // tx service wants checksummed
    let nonce = value_str(&get_json(c, &format!("{TX_SERVICE}/safes/{addr}/")).await?["nonce"]);
    // ponytail: first page only (100 txs), follow `next` if a Safe ever queues more
    let url = format!("{TX_SERVICE}/safes/{addr}/multisig-transactions/?executed=false&nonce__gte={nonce}&ordering=nonce&limit=100");
    let page = get_json(c, &url).await?;
    Ok(page["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({
            "safe": safe,
            "safeTxHash": t["safeTxHash"],
            "nonce": t["nonce"],
            "to": t["to"],
            "value": value_str(&t["value"]),
            "method": t["dataDecoded"]["method"],
            "confirmations": t["confirmations"].as_array().map_or(0, Vec::len),
            "confirmationsRequired": t["confirmationsRequired"],
            "submissionDate": t["submissionDate"],
        }))
        .collect())
}

fn rpc_url() -> Option<String> {
    let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
    var("MAINNET_RPC_URL").or_else(|| var("ETH_RPC_URL"))
}

async fn get_json(c: &Client, url: &str) -> Res<Value> {
    Ok(c.get(url).timeout(Duration::from_secs(30)).send().await?.error_for_status()?.json().await?)
}

async fn rpc(c: &Client, url: &str, method: &str, params: Value) -> Res<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut out: Value =
        c.post(url).timeout(Duration::from_secs(60)).json(&body).send().await?.error_for_status()?.json().await?;
    if let Some(e) = out.get("error") {
        return Err(e.get("message").map_or_else(|| e.to_string(), value_str).into());
    }
    Ok(out["result"].take())
}

sol! {
    interface IERC20 {
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
}

/// token -> (symbol, decimals), in one Multicall3 round-trip.
/// Unreadable metadata -> (address, 0): raw units.
async fn token_metas(url: &str, tokens: &[&String]) -> HashMap<String, (String, u8)> {
    let mut meta: HashMap<String, (String, u8)> = tokens.iter().map(|&t| (t.clone(), (t.clone(), 0))).collect();
    meta.insert("ETH".into(), ("ETH".into(), 18));
    let erc20: Vec<(&String, Address)> = tokens.iter().filter_map(|&t| Some((t, t.parse().ok()?))).collect();
    let Ok(url) = url.parse() else { return meta };
    if erc20.is_empty() {
        return meta;
    }
    let p = ProviderBuilder::new().connect_http(url);
    let symbols = p.multicall().dynamic::<IERC20::symbolCall>()
        .extend_calls(erc20.iter().map(|&(_, a)| CallItem::new(a, IERC20::symbolCall {}.abi_encode().into()).with_failure_allowed()));
    let decimals = p.multicall().dynamic::<IERC20::decimalsCall>()
        .extend_calls(erc20.iter().map(|&(_, a)| CallItem::new(a, IERC20::decimalsCall {}.abi_encode().into()).with_failure_allowed()));
    let (Ok(symbols), Ok(decimals)) = tokio::join!(symbols.aggregate3(), decimals.aggregate3()) else { return meta };
    for ((t, _), (s, d)) in erc20.iter().zip(symbols.into_iter().zip(decimals)) {
        // bytes32 symbols (MKR) fail the string decode; alloy hands back the raw bytes
        let s = s.unwrap_or_else(|f| decode_string(&hex::encode(&f.return_data)));
        if let Ok(d) = d {
            meta.insert(t.to_string(), (if s.is_empty() { t.to_string() } else { s }, d));
        }
    }
    meta
}

async fn token_summary(c: &Client, url: &str, from: &str, to: &str, calldata: &str, overrides: Value) -> Res<Value> {
    let root = rpc(c, url, "debug_traceCall", json!([
        {"from": from, "to": to, "data": calldata}, "latest",
        {"tracer": "callTracer", "tracerConfig": {"withLog": true}, "stateOverrides": overrides},
    ]))
    .await?;
    let moves = extract_movements(&root);
    let mut tokens: Vec<&String> = moves.iter().map(|(t, ..)| t).filter(|t| *t != "ETH").collect();
    tokens.sort();
    tokens.dedup();
    let meta = token_metas(url, &tokens).await;
    let transfers: Vec<Value> = moves
        .iter()
        .map(|(t, a, b, n)| json!({"token": t, "symbol": meta[t].0, "from": a, "to": b,
                                   "amount": fmt_amount(to_i512(*n), meta[t].1)}))
        .collect();
    let mut net: Vec<_> = net_balances(&moves).into_iter().collect();
    net.sort_by(|((t1, h1), _), ((t2, h2), _)| (h1, t1).cmp(&(h2, t2)));
    let balances: Vec<Value> = net
        .iter()
        .map(|((t, h), d)| json!({"holder": h, "token": t, "symbol": meta[t].0,
            "delta": format!("{}{}", if d.is_positive() { "+" } else { "" }, fmt_amount(*d, meta[t].1))}))
        .collect();
    Ok(json!({"transfers": transfers, "balances": balances}))
}

/// Simulate the Safe tx; return its summary plus the decoded cast trace.
///
/// A revert is a useful result, not an error: it comes back in `trace` with
/// `success=false`.
async fn simulate(c: &Client, safe_tx_hash: &str, url: &str) -> Res<Value> {
    let book = load_address_book()?;
    let tx = get_json(c, &format!("{TX_SERVICE}/multisig-transactions/{safe_tx_hash}/")).await?;
    let safe = value_str(&tx["safe"]);
    let owner = get_json(c, &format!("{TX_SERVICE}/safes/{safe}/")).await?["owners"][0]
        .as_str()
        .ok_or("Safe has no owners.")?
        .to_string();
    let data = tx["data"].as_str().filter(|d| !d.is_empty()).unwrap_or("0x");

    let field = |k: &str| value_str(&tx[k]);
    let args = [
        field("to"),
        field("value"),
        data.to_string(),
        field("operation"),
        field("safeTxGas"),
        field("baseGas"),
        field("gasPrice"),
        field("gasToken"),
        field("refundReceiver"),
        build_signature(&owner),
    ];
    let cast = tokio::process::Command::new("cast")
        .args(["call", &safe, EXEC_SIG])
        .args(&args)
        .args(["--from", &owner])
        .args(["--override-state-diff", &format!("{safe}:{THRESHOLD_SLOT}:{THRESHOLD_ONE}")])
        .arg("--trace")
        .args(book.iter().map(|(a, label)| format!("--labels={a}:{label}")))
        .args(["--rpc-url", url])
        .env("NO_COLOR", "1")
        .output();

    // Same call again via the node's debug_traceCall: structured logs to sum up.
    let tokens = async {
        let calldata = exec_calldata(&args)?;
        let slot = format!("0x{:0>64}", &THRESHOLD_SLOT[2..]);
        let overrides = json!({ safe.as_str(): {"stateDiff": { slot: THRESHOLD_ONE }}});
        Res::Ok(token_summary(c, url, &owner, &safe, &calldata, overrides).await?)
    };
    let (r, tokens) = tokio::join!(cast, tokens);
    let r = r.map_err(|e| format!("Running cast failed (is Foundry installed?): {e}"))?;
    // RPC may lack debug_*; trace still useful
    let mut out = tokens.unwrap_or_else(|e| json!({"transfers": [], "balances": [], "tokensError": e.to_string()}));

    let (stdout, stderr) = (String::from_utf8_lossy(&r.stdout), String::from_utf8_lossy(&r.stderr));
    let success = r.status.success();
    let rest = json!({
        "safe": safe,
        "to": tx["to"],
        "value": field("value"),
        "data": data,
        "operation": tx["operation"],
        "nonce": tx["nonce"],
        "owner": owner,
        "labels": book,
        "success": success,
        "trace": if success { stdout.to_string() } else { format!("{stdout}{stderr}") },
    });
    out.as_object_mut().unwrap().extend(rest.as_object().unwrap().clone());
    Ok(out)
}

/// Every API answer is 200 JSON: {"ok": true, ...} or {"ok": false, "error"}.
fn reply(r: Res<Value>) -> Json<Value> {
    Json(match r {
        Ok(Value::Object(mut m)) => {
            m.insert("ok".into(), true.into());
            Value::Object(m)
        }
        Ok(v) => v,
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    })
}

fn json_body(h: &HeaderMap, body: &[u8]) -> Res<Value> {
    // Non-simple content type forces a CORS preflight (which we never
    // answer), so other websites can't POST here from your browser.
    let ctype = h.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ctype.split(';').next() != Some("application/json") {
        return Err("Content-Type must be application/json.".into());
    }
    Ok(if body.is_empty() { json!({}) } else { serde_json::from_slice(body)? })
}

fn static_file(name: &str, ctype: &'static str) -> Response {
    match std::fs::read(here("static").join(name)) {
        Ok(b) => ([(CONTENT_TYPE, ctype)], b).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn get_book() -> Json<Value> {
    reply(load_address_book().map(|book| json!({"book": book})))
}

async fn post_book(h: HeaderMap, body: Bytes) -> Json<Value> {
    reply((|| {
        let body = json_body(&h, &body)?;
        let book = body["book"].as_object().ok_or("Expected {\"book\": {address: label}}.")?;
        Ok(json!({"book": save_address_book(book)?}))
    })())
}

async fn get_safes() -> Json<Value> {
    reply(load_safes().map(|safes| json!({"safes": safes})))
}

async fn post_safes(h: HeaderMap, body: Bytes) -> Json<Value> {
    reply((|| {
        let body = json_body(&h, &body)?;
        let safes = body["safes"].as_array().ok_or("Expected {\"safes\": [address, ...]}.")?;
        Ok(json!({"safes": save_safes(safes)?}))
    })())
}

/// Pending txs of every saved Safe; one failing Safe doesn't hide the others.
async fn get_pending(State(c): State<Client>) -> Json<Value> {
    reply(
        async {
            let (mut pending, mut errors) = (Vec::new(), Vec::new());
            // ponytail: sequential, one Safe at a time; join_all if the list gets long
            for safe in load_safes()? {
                match pending_for(&c, &safe).await {
                    Ok(txs) => pending.extend(txs),
                    Err(e) => errors.push(json!({"safe": safe, "error": e.to_string()})),
                }
            }
            Ok(json!({"pending": pending, "errors": errors}))
        }
        .await,
    )
}

async fn post_simulate(State(c): State<Client>, h: HeaderMap, body: Bytes) -> Json<Value> {
    reply(
        async {
            let raw = value_str(&json_body(&h, &body)?["safeTxHash"]);
            // accept a bare hash or a full app.safe.global link (hash is the last 32-byte hex)
            let re = Regex::new(r"0x[0-9a-fA-F]{64}").unwrap();
            let hash = re.find_iter(&raw).last().ok_or("No safeTxHash (0x + 64 hex) found in input.")?;
            let url = rpc_url().ok_or("Set MAINNET_RPC_URL and restart the server.")?;
            simulate(&c, hash.as_str(), &url).await
        }
        .await,
    )
}

#[tokio::main]
async fn main() {
    println!("Serving on http://localhost:8000  (Ctrl-C to stop)");
    if rpc_url().is_none() {
        println!("WARNING: MAINNET_RPC_URL not set.");
    }
    let app = Router::new()
        .route("/", get(|| async { static_file("index.html", "text/html; charset=utf-8") }))
        .route("/index.html", get(|| async { static_file("index.html", "text/html; charset=utf-8") }))
        .route("/app.js", get(|| async { static_file("app.js", "text/javascript; charset=utf-8") }))
        .route("/addressbook", get(get_book).post(post_book))
        .route("/safes", get(get_safes).post(post_safes))
        .route("/pending", get(get_pending))
        .route("/simulate", post(post_simulate))
        .with_state(Client::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000").await.expect("bind 127.0.0.1:8000");
    axum::serve(listener, app).await.expect("server error");
}
