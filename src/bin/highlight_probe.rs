// Replays quick-answer cases saved with DUMP_HIGHLIGHT_DIR through the current
// `highlight::extract`, and prints where the answer changed.
//
//   cargo run --release --bin highlight_probe                # tests/fixtures/highlight
//   cargo run --release --bin highlight_probe -- DIR --bless  # record current answers
use searchwala::models::SourceResult;
use serde_json::Value;

fn sources(v: &Value) -> Vec<SourceResult> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let text = s["extracted_text"].as_str().unwrap_or("").to_string();
            SourceResult {
                url: s["url"].as_str().unwrap_or("").into(),
                title: s["title"].as_str().unwrap_or("").into(),
                char_count: text.chars().count(),
                extracted_text: text,
                engine: s["engine"].as_str().unwrap_or("").into(),
                ..Default::default()
            }
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bless = args.iter().any(|a| a == "--bless");
    let dir = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "tests/fixtures/highlight".into());
    let mut paths: Vec<_> = std::fs::read_dir(&dir).expect("case dir").flatten().map(|e| e.path()).collect();
    paths.sort();
    let (mut same, mut changed) = (0, 0);
    for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let query = v["query"].as_str().unwrap().to_string();
        let query = query.as_str();
        let intel = searchwala::query_intel::analyze_query(v["intel_query"].as_str().unwrap_or(query));
        let got = searchwala::highlight::extract(query, &intel, &sources(&v["results"]), &sources(&v["leads"])).map(|h| h.text);
        let before = v["highlight"].as_str().map(str::to_string);
        if bless && got != before {
            v["highlight"] = got.clone().into();
            std::fs::write(p, v.to_string()).unwrap();
        }
        if got == before {
            same += 1;
            println!("SAME    {query}\n        {}", before.as_deref().unwrap_or("(none)"));
        } else {
            changed += 1;
            println!("CHANGED {query}\n  was:  {}\n  now:  {}", before.as_deref().unwrap_or("(none)"), got.as_deref().unwrap_or("(none)"));
        }
    }
    println!("\n{same} unchanged, {changed} changed");
}
