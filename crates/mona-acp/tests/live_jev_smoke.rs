//! Explicit live check: never runs during the ordinary test suite.
use mona_acp::live_jev::LiveJevClassifier;
use mona_jev::{JevClassifier, JevClassifyRequest};

#[tokio::test]
#[ignore = "requires an explicitly configured live Jev account and makes a network request"]
async fn live_jev_classifies_a_bounded_prompt() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let classifier = LiveJevClassifier::from_config().expect("live Jev configuration");
    let plan = classifier
        .classify(&JevClassifyRequest {
            prompt: "Answer a simple greeting. No tools or changes are needed.".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![
                "gpt-5.6-luna".into(),
                "gpt-5.6-terra".into(),
                "gpt-5.6-sol".into(),
                "gpt-6-astra".into(),
            ],
            available_efforts: vec!["low".into(), "medium".into(), "high".into()],
        })
        .await
        .expect("live Jev returned a usable routing decision");
    assert!(!plan.sensitive, "a greeting must not need review");
    assert_eq!(plan.rationale, "live Jev typed Decisions route");
    println!(
        "live Jev verified: tier={:?}, effort={:?}, confidence={}",
        plan.tier, plan.effort, plan.confidence
    );
}
