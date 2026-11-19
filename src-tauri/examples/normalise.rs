//! Prints what the normaliser makes of each input, for the pronunciation
//! eval (`tests/pronunciation/eval.py`). Reads a JSON array of strings on
//! stdin and writes a JSON array of the normalised strings to stdout.
//!
//! `--off rule,rule` switches rule groups off (names from `Rule::name`), so
//! the eval can check what each one is worth.

use std::io::Read;

use vox_lib::normalise::{normalise_with, Rule};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let off: Vec<String> = args
        .iter()
        .position(|a| a == "--off")
        .and_then(|i| args.get(i + 1))
        .map(|list| list.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    for name in &off {
        assert!(
            Rule::ALL.iter().any(|r| r.name() == name),
            "unknown rule {name}"
        );
    }
    let rules: Vec<Rule> = Rule::ALL
        .into_iter()
        .filter(|r| !off.iter().any(|o| o == r.name()))
        .collect();

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read stdin");
    let texts: Vec<String> = serde_json::from_str(&input).expect("a JSON array of strings");
    let out: Vec<String> = texts.iter().map(|t| normalise_with(t, &rules)).collect();
    println!("{}", serde_json::to_string(&out).expect("serialise"));
}
