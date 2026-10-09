//! Feature-gated native Core protocol fixture; no engine or credentials.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};
fn main() {
    let arguments = std::env::args().collect::<Vec<_>>();
    let exit_code = if arguments.iter().any(|arg| arg == "exit-on-list") {
        Some(23)
    } else if arguments.iter().any(|arg| arg == "exit-zero-on-list") {
        Some(0)
    } else {
        None
    };
    let mut counts = BTreeMap::<String, u64>::new();
    let mut turns = Vec::<Value>::new();
    let mut keys = BTreeMap::<String, Value>::new();
    let mut event_mode = String::new();
    let mut events = vec![
        json!({"sequence":1,"thread_id":"thread-1","turn_id":null,"kind":"thread.started","data":{},"created_at":1}),
    ];
    let thread = json!({"id":"thread-1","title":"Fixture","agent_id":"fixture","bundle_digest":"sha256:fixture","release_id":"sha256:release","sandbox_id":null,"runtime_instance_id":"instance-1","runtime_image":"fixture:latest","runtime_provider":"docker","workdir":"/workspace","created_at":1,"updated_at":1,"last_sequence":1});
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        *counts.entry(method.into()).or_default() += 1;
        if method == "agent/list"
            && let Some(code) = exit_code
        {
            std::process::exit(code);
        }
        let result: Result<Value, (i32, &str)> = match method {
            "initialize" => {
                Ok(json!({"protocol_version":"1.0","server_name":"fixture","capabilities":[]}))
            }
            "agent/list" => Ok(
                json!({"agents":[{"id":"fixture","name":"Fixture","model":"test","bundle_digest":"sha256:fixture","release_id":"sha256:release","runtime":{"provider":"docker","images":["fixture:latest","fixture:alternate"],"default_image":"fixture:latest","default_workdir":"/workspace"}}]}),
            ),
            "runtime/instances" => {
                let limit = params["limit"].as_u64().unwrap_or(50);
                if params["agent_id"] != "fixture" {
                    Err((-32004, "agent not found"))
                } else if !(1..=100).contains(&limit) {
                    Err((-32602, "invalid instance page limit"))
                } else if params["cursor"].is_string() && params["cursor"] != "instance-1" {
                    Err((-32004, "instance cursor not found"))
                } else {
                    let instances = if params["cursor"] == "instance-1" {
                        vec![]
                    } else {
                        vec![json!({
                            "id":"instance-1","agent_id":"fixture","release_id":"sha256:release","bundle_digest":"sha256:fixture",
                            "provider":"docker","image":"fixture:latest","sandbox_id":"sandbox-1","workdir":"/workspace",
                            "created_at":1,"updated_at":2,"busy":true
                        })]
                    };
                    Ok(json!({"instances":instances,"next_cursor":null}))
                }
            }
            "thread/start" => {
                if params["agent_id"] == "fixture" {
                    let mut created = thread.clone();
                    if let Some(workdir) = params.get("workdir").filter(|v| v.is_string()) {
                        created["workdir"] = workdir.clone();
                    }
                    if params["runtime"]["mode"] == "new" {
                        if let Some(image) =
                            params["runtime"].get("image").filter(|v| v.is_string())
                        {
                            created["runtime_image"] = image.clone();
                        }
                    } else if params["runtime"]["mode"] == "reuse" {
                        created["runtime_instance_id"] = params["runtime"]["instance_id"].clone();
                        created["sandbox_id"] = json!("sandbox-1");
                    }
                    Ok(created)
                } else {
                    Err((-32004, "agent not found"))
                }
            }
            "thread/list" => Ok(json!({"threads":[thread],"next_cursor":null})),
            "thread/read" => Ok(json!({"thread":thread,"turns":turns,"next_cursor":null})),
            "turn/start" => {
                let key = params["idempotency_key"].as_str().unwrap();
                if let Some(turn) = keys.get(key) {
                    if turn["input"] == params["input"] {
                        Ok(turn.clone())
                    } else {
                        Err((-32009, "idempotency conflict"))
                    }
                } else {
                    let turn = json!({"id":format!("turn-{}",turns.len()+1),"thread_id":"thread-1","input":params["input"],"status":"running","output":null,"error":null,"created_at":2,"updated_at":2});
                    turns.push(turn.clone());
                    keys.insert(key.into(), turn.clone());
                    events.push(json!({"sequence":events.len()+1,"thread_id":"thread-1","turn_id":turn["id"],"kind":"turn.started","data":{},"created_at":2}));
                    Ok(turn)
                }
            }
            "turn/interrupt" => {
                events.push(json!({"sequence":events.len()+1,"thread_id":"thread-1","turn_id":params["turn_id"],"kind":"turn.interrupted","data":{},"created_at":3}));
                if let Some(turn) = turns
                    .iter_mut()
                    .find(|turn| turn["id"] == params["turn_id"])
                {
                    turn["status"] = json!("interrupted");
                    turn["updated_at"] = json!(3);
                    Ok(turn.clone())
                } else {
                    Err((-32004, "turn not found"))
                }
            }
            "item/list" => {
                let after = params["after"].as_u64().unwrap_or(0);
                if params["thread_id"] != "thread-1"
                    || !turns.iter().any(|turn| turn["id"] == params["turn_id"])
                {
                    Err((-32004, "turn not found in thread"))
                } else if after > i64::MAX as u64 {
                    Err((-32602, "item cursor exceeds supported range"))
                } else {
                    let items = if after < 3 {
                        vec![
                            json!({"id":"assistant-1","kind":"assistant_message","status":"completed",
                            "phase":"commentary","name":null,"text":"Inspecting workspace","input":null,
                            "output":null,"error":null,"elapsed_ms":null,"truncated":false,
                            "turn_id":params["turn_id"],"ordinal":3,"sequence":4,"created_at":2,"updated_at":3}),
                        ]
                    } else {
                        vec![]
                    };
                    Ok(
                        json!({"next_cursor":if items.is_empty() {after} else {3},"has_more":false,"items":items}),
                    )
                }
            }
            "event/list" if event_mode == "error" => {
                Err((-32010, "fixture temporarily unavailable"))
            }
            "event/list" if event_mode == "skip" => {
                Ok(json!({"events":[],"next_cursor":10000,"has_more":false}))
            }
            "event/list" => {
                let after = params["after"].as_u64().unwrap_or(0);
                let page = events
                    .iter()
                    .filter(|event| event["sequence"].as_u64().unwrap() > after)
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(
                    json!({"events":page,"next_cursor":events.len().max(after as usize),"has_more":false}),
                )
            }
            "fixture/stats" => Ok(json!({"methods":counts})),
            "fixture/event-mode" => {
                event_mode = params["mode"].as_str().unwrap().into();
                Ok(json!({}))
            }
            "shutdown" => Ok(json!({})),
            _ => Err((-32601, "method not found")),
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":message}})
            }
        };
        println!("{response}");
        io::stdout().flush().unwrap();
        if method == "shutdown" {
            break;
        }
    }
}
