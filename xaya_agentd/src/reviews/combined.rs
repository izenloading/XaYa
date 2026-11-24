//! Combine independent decisions on the same checkpoint into one task review.
use std::collections::BTreeMap;

use xaya_agent_proto::{CriterionStatus, ReviewEngine, ReviewVerdict};

use super::verdict::Decision;
use crate::tasks::ledger_key;

pub(super) fn merge(mut decisions: Vec<(ReviewEngine, Decision)>) -> Decision {
    if decisions.len() == 1 {
        return decisions.pop().map(|(_, decision)| decision).unwrap_or_default();
    }
    let mut merged = Decision::default();
    let mut findings = BTreeMap::new();
    let mut criteria = BTreeMap::new();
    for (engine, decision) in decisions {
        if verdict_rank(decision.verdict) > verdict_rank(merged.verdict) {
            merged.verdict = decision.verdict;
        }
        merged.reasons.extend(
            decision.reasons.into_iter().map(|reason| format!("{}: {reason}", engine.as_str())),
        );
        merged.dropped.extend(decision.dropped);
        if merged.overall_correct.is_none() || decision.overall_correct == Some(false) {
            merged.overall_correct = decision.overall_correct;
            merged.overall_explanation = decision.overall_explanation;
        }
        for finding in decision.findings {
            let key = ledger_key(&finding.path, &finding.title);
            match findings.get(&key).copied() {
                Some(index) => {
                    let existing: &xaya_agent_proto::ReviewFinding = &merged.findings[index];
                    let strength = |f: &xaya_agent_proto::ReviewFinding| {
                        (f.blocking, std::cmp::Reverse(f.priority.unwrap_or(u8::MAX)), f.confidence)
                    };
                    if strength(&finding) > strength(existing) {
                        merged.findings[index] = finding;
                    }
                },
                None => {
                    findings.insert(key, merged.findings.len());
                    merged.findings.push(finding);
                },
            }
        }
        for result in decision.criteria {
            match criteria.get(&result.id).copied() {
                Some(index) => {
                    let existing: &xaya_agent_proto::CriterionResult = &merged.criteria[index];
                    if criterion_rank(result.status) > criterion_rank(existing.status) {
                        merged.criteria[index] = result;
                    }
                },
                None => {
                    criteria.insert(result.id.clone(), merged.criteria.len());
                    merged.criteria.push(result);
                },
            }
        }
    }
    merged
}

fn verdict_rank(verdict: Option<ReviewVerdict>) -> u8 {
    match verdict {
        None => 0,
        Some(ReviewVerdict::Approved) => 1,
        Some(ReviewVerdict::Blocked) => 2,
        Some(ReviewVerdict::NeedsFix) => 3,
        Some(ReviewVerdict::Invalid) => 4,
    }
}

fn criterion_rank(status: CriterionStatus) -> u8 {
    match status {
        CriterionStatus::Met => 0,
        CriterionStatus::Unknown => 1,
        CriterionStatus::Unmet => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xaya_agent_proto::ReviewFinding;

    fn answer(verdict: ReviewVerdict, findings: Vec<ReviewFinding>) -> Decision {
        Decision { verdict: Some(verdict), findings, ..Decision::default() }
    }

    fn finding(title: &str, path: &str, blocking: bool) -> ReviewFinding {
        ReviewFinding {
            title: title.into(),
            body: "description".into(),
            priority: Some(if blocking { 1 } else { 2 }),
            confidence: 0.95,
            path: path.into(),
            start: 1,
            end: 1,
            blocking,
        }
    }

    #[test]
    fn both_engines_must_approve() {
        use ReviewVerdict::{Approved, Blocked, Invalid, NeedsFix};
        for (first, second, expected) in [
            (Approved, Approved, Approved),
            (Approved, Blocked, Blocked),
            (Approved, NeedsFix, NeedsFix),
            (NeedsFix, Invalid, Invalid),
        ] {
            let decision = merge(vec![
                (ReviewEngine::Codex, answer(first, vec![])),
                (ReviewEngine::Claude, answer(second, vec![])),
            ]);
            assert_eq!(decision.verdict, Some(expected));
        }
    }

    #[test]
    fn duplicate_findings_use_the_stronger_result_and_keep_distinct_files() {
        let decision = merge(vec![
            (
                ReviewEngine::Codex,
                answer(ReviewVerdict::Approved, vec![finding("[P2] Mean fails", "a.py", false)]),
            ),
            (
                ReviewEngine::Claude,
                answer(ReviewVerdict::NeedsFix, vec![
                    finding("[P1] Mean fails", "a.py", true),
                    finding("Mean fails", "b.py", true),
                ]),
            ),
        ]);
        assert_eq!(decision.findings.len(), 2);
        assert_eq!(decision.findings[0].path, "a.py");
        assert!(decision.findings[0].blocking);
        assert_eq!(decision.findings[1].path, "b.py");
    }
}
