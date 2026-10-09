//! Native JSON-RPC fixture executable. It never loads an agent or credentials.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
fn main() {
    let mut shutdown_behavior = String::new();
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap();
        if method == "slow" {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if method == "configure_shutdown" {
            shutdown_behavior = request["params"]["behavior"].as_str().unwrap().into();
        }
        if method == "shutdown" {
            match shutdown_behavior.as_str() {
                "ignore" => continue,
                "slow" => std::thread::sleep(std::time::Duration::from_millis(250)),
                "error" => {
                    println!(
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Core shutdown persistence failed"}})
                    );
                    io::stdout().flush().unwrap();
                    break;
                }
                _ => {}
            }
        }
        match method {
            "exit" => std::process::exit(7),
            "hang" => continue,
            "invalid" => {
                println!("not-json");
                io::stdout().flush().unwrap();
                continue;
            }
            "oversize" => {
                println!("{}", "x".repeat(8192));
                io::stdout().flush().unwrap();
                continue;
            }
            "error" => println!(
                "{}",
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32004,"message":"not found"}})
            ),
            _ => println!(
                "{}",
                json!({"jsonrpc":"2.0","id":id,"result":request["params"]})
            ),
        }
        io::stdout().flush().unwrap();
        if method == "shutdown" {
            break;
        }
    }
}
