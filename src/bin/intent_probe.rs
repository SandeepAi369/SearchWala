fn main() {
    let raw = std::fs::read_to_string("benchmarks/golden_queries.json").unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut wrong = Vec::new();
    let mut total = 0;
    let mut ok = 0;
    for q in v["queries"].as_array().unwrap() {
        let query = q["query"].as_str().unwrap();
        let want = q["expected_intent"].as_str().unwrap_or("");
        if want.is_empty() { continue }
        total += 1;
        let got = searchwala::query_intel::analyze_query(query).intent.to_string();
        if got == want { ok += 1 } else { wrong.push((query.to_string(), want.to_string(), got)) }
    }
    println!("intent accuracy: {}/{} = {:.0}%", ok, total, ok as f64 / total as f64 * 100.0);
    println!("\nMISCLASSIFIED:");
    for (q, want, got) in &wrong {
        println!("  want={:<14} got={:<14} {}", want, got, q);
    }
}
