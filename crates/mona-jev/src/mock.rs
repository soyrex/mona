//! In-memory mock classifier for tests and offline use.
//!
//! Returns a configurable [`JevRoutePlan`] regardless of input. Used by
//! `crates/mona-acp/tests/end_to_end.rs` to assert that the harness correctly
//! applies whatever plan the classifier produces, without coupling the
//! integration test to the rule-based heuristics.

use crate::{JevClassifyRequest, JevClassifier, JevError, JevRoutePlan};
use async_trait::async_trait;
use std::sync::Mutex;
use uuid::Uuid;

/// Mock classifier. Stores the most recent request for test inspection
/// and returns a preset plan (defaulting to Balanced).
pub struct MockJevClassifier {
    last_request: Mutex<Option<JevClassifyRequest>>,
    plan_to_return: Mutex<JevRoutePlan>,
}

impl MockJevClassifier {
    pub fn new() -> Self {
        Self {
            last_request: Mutex::new(None),
            plan_to_return: Mutex::new(JevRoutePlan::passthrough(
                crate::ModelTier::Balanced,
                Some("medium".into()),
            )),
        }
    }

    /// Configure the plan the next classify call will return.
    pub fn set_plan(&self, plan: JevRoutePlan) {
        *self.plan_to_return.lock().unwrap() = plan;
    }

    /// Inspect the most recent request received by the classifier.
    pub fn last_request(&self) -> Option<JevClassifyRequest> {
        self.last_request.lock().unwrap().clone()
    }
}

impl Default for MockJevClassifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl JevClassifier for MockJevClassifier {
    async fn classify(&self, req: &JevClassifyRequest) -> Result<JevRoutePlan, JevError> {
        if req.prompt.trim().is_empty() {
            return Err(JevError::EmptyPrompt);
        }
        *self.last_request.lock().unwrap() = Some(req.clone());

        let mut plan = self.plan_to_return.lock().unwrap().clone();
        // Fresh trace_id per call so each classification is distinguishable.
        plan.trace_id = Uuid::new_v4();
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_last_request() {
        let m = MockJevClassifier::new();
        let req = JevClassifyRequest {
            prompt: "hi".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        m.classify(&req).await.unwrap();
        let last = m.last_request().unwrap();
        assert_eq!(last.prompt, "hi");
    }

    #[tokio::test]
    async fn mock_returns_configured_plan() {
        let m = MockJevClassifier::new();
        m.set_plan(JevRoutePlan::passthrough(
            crate::ModelTier::Frontier,
            Some("max".into()),
        ));
        let req = JevClassifyRequest {
            prompt: "x".into(),
            recent_messages: vec![],
            last_turn_outcome: None,
            available_models: vec![],
            available_efforts: vec![],
        };
        let plan = m.classify(&req).await.unwrap();
        assert_eq!(plan.tier, crate::ModelTier::Frontier);
    }
}
