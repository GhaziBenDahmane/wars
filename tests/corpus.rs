//! Every prompt seen in real races: verified answers must be reproduced, and
//! answers the server rejected must never be given again.

use agentwars::solvers::solve;
use serde_json::Value;

#[test]
fn reproduces_the_verified_corpus() {
    let corpus = include_str!("../data/agentwars_prompts.jsonl");
    let (mut checked, mut failures) = (0, Vec::new());
    for line in corpus.lines().filter(|l| !l.trim().is_empty()) {
        let entry: Value = serde_json::from_str(line).unwrap();
        let prompt = entry["prompt"].as_str().unwrap();
        let submission = entry["submission"].as_str().unwrap();
        let answer = solve(prompt);
        let correct = entry["correct"].as_bool() == Some(true);
        if correct && answer.as_deref() != Some(submission) {
            failures.push(format!("expected {submission:?}, got {answer:?}: {prompt}"));
        } else if !correct && answer.as_deref() == Some(submission) {
            failures.push(format!("repeats rejected {submission:?}: {prompt}"));
        }
        checked += 1;
    }
    assert!(checked > 500, "corpus too small: {checked}");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
