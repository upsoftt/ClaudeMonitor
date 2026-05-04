//! Parses `last_usage_response.json` (written by the running app) into our
//! `UsageResponse` struct and prints the result. Tells us instantly whether
//! the deserialization itself is dropping `seven_day` / `seven_day_omelette`.

use claude_monitor::types::UsageResponse;
use std::env;

fn main() {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "../last_usage_response.json".to_string());
    let body = std::fs::read_to_string(&path).expect("read dump");
    println!("RAW BYTES = {}\n", body.len());
    let parsed: UsageResponse = serde_json::from_str(&body).expect("parse");
    println!("five_hour            = {:?}", parsed.five_hour);
    println!("seven_day            = {:?}", parsed.seven_day);
    println!("seven_day_omelette   = {:?}", parsed.seven_day_omelette);
    println!("seven_day_opus       = {:?}", parsed.seven_day_opus);
    println!("seven_day_sonnet     = {:?}", parsed.seven_day_sonnet);
    println!("seven_day_cowork     = {:?}", parsed.seven_day_cowork);
    println!("seven_day_oauth_apps = {:?}", parsed.seven_day_oauth_apps);
}
