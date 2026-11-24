//! The service's verdict on a review. The reviewer's structured output is one input; the rules
//! here decide, and no wording in the reviewed change can talk them into an approval.
use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Deserialize;
use xaya_agent_proto::{
    Criterion, CriterionResult, CriterionStatus, DroppedFinding, ReviewFinding, ReviewVerdict,
};

/// Reviewer output larger than this is not parsed.
pub(super) const MAX_OUTPUT: usize = 256 * 1024;
const MAX_FINDINGS: usize = 50;
/// A finding may sit this many lines off a changed range and still be about it.
const TOLERANCE: u32 = 3;
const MIN_BLOCKING_CONFIDENCE: f64 = 0.5;

/// The schema both engines must answer with; Codex requires every property and no extras. A
/// task review also requires one result per acceptance criterion.
pub(super) fn schema(with_criteria: bool) -> serde_json::Value {
    let mut schema = base_schema();
    if with_criteria {
        schema["required"].as_array_mut().expect("required list").push("criteria".into());
        schema["properties"]["criteria"] = serde_json::json!({"type": "array", "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "status", "evidence"],
            "properties": {
                "id": {"type": "string"},
                "status": {"type": "string", "enum": ["met", "unmet", "unknown"]},
                "evidence": {"type": "string"},
            },
        }});
    }
    schema
}

fn base_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["findings", "overall_correctness", "overall_explanation",
                     "overall_confidence_score"],
        "properties": {
            "findings": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "body", "confidence_score", "priority", "code_location"],
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "confidence_score": {"type": "number"},
                    "priority": {"type": ["integer", "null"]},
                    "code_location": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["absolute_file_path", "line_range"],
                        "properties": {
                            "absolute_file_path": {"type": "string"},
                            "line_range": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["start", "end"],
                                "properties": {
                                    "start": {"type": "integer"},
                                    "end": {"type": "integer"},
                                },
                            },
                        },
                    },
                },
            }},
            "overall_correctness": {"type": "string",
                                    "enum": ["patch is correct", "patch is incorrect"]},
            "overall_explanation": {"type": "string"},
            "overall_confidence_score": {"type": "number"},
        },
    })
}

#[derive(Deserialize)]
struct Output {
    findings: Vec<Finding>,
    overall_correctness: String,
    overall_explanation: String,
    #[allow(dead_code)]
    overall_confidence_score: f64,
    #[serde(default)]
    criteria: Option<Vec<CriterionOutput>>,
}

#[derive(Deserialize)]
struct CriterionOutput {
    id: String,
    status: CriterionStatus,
    evidence: String,
}

#[derive(Deserialize)]
struct Finding {
    title: String,
    body: String,
    confidence_score: f64,
    priority: Option<i64>,
    code_location: Location,
}

#[derive(Deserialize)]
struct Location {
    absolute_file_path: String,
    line_range: Range,
}

#[derive(Deserialize)]
struct Range {
    start: i64,
    end: i64,
}

/// New-side line ranges each changed path occupies in the reviewed diff. `None` for a path's
/// ranges means only the path is known (the diff was too large to locate lines).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Changes(pub BTreeMap<String, Option<Vec<(u32, u32)>>>);

impl Changes {
    pub(super) fn paths_only(paths: impl IntoIterator<Item = String>) -> Self {
        Self(paths.into_iter().map(|path| (path, None)).collect())
    }

    /// From `git diff -U0 --no-renames` output.
    pub(super) fn from_unified_zero(diff: &str) -> Self {
        let mut changes = BTreeMap::<String, Option<Vec<(u32, u32)>>>::new();
        let mut current: Option<String> = None;
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ ") {
                current = path.strip_prefix("b/").map(str::to_owned);
                if let Some(path) = &current {
                    changes.entry(path.clone()).or_insert_with(|| Some(Vec::new()));
                }
            } else if let (Some(hunk), Some(path)) = (line.strip_prefix("@@ "), &current) {
                let Some(new) = hunk.split(' ').find_map(|part| part.strip_prefix('+')) else {
                    continue;
                };
                let mut numbers = new.split(',');
                let start: u32 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let count: u32 = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                // A pure deletion has no new lines; it sits between line `start` and the next.
                let range = if count == 0 {
                    (start.max(1), start.max(1))
                } else {
                    (start, start + count - 1)
                };
                if let Some(Some(ranges)) = changes.get_mut(path) {
                    ranges.push(range);
                }
            }
        }
        Self(changes)
    }
}

/// Whether the bound verification lets an approval through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verification {
    Missing,
    Passed,
    NotPassing(String),
}

pub(super) struct Input<'a> {
    /// The reviewer's final structured answer, if it produced one.
    pub output: Option<&'a str>,
    /// The reviewer's private checkout no longer matched the reviewed commit.
    pub checkout_changed: bool,
    pub checkout: &'a Path,
    pub changes: &'a Changes,
    /// Whether a repository-relative path is a regular file in the reviewed commit.
    pub exists: &'a dyn Fn(&str) -> bool,
    pub verification: Verification,
    pub diff_truncated: bool,
    /// The task's acceptance criteria, when the review is for a task.
    pub criteria: Option<&'a [Criterion]>,
}

#[derive(Debug, Default)]
pub(super) struct Decision {
    pub verdict: Option<ReviewVerdict>,
    pub reasons: Vec<String>,
    pub findings: Vec<ReviewFinding>,
    pub dropped: Vec<DroppedFinding>,
    pub overall_correct: Option<bool>,
    pub overall_explanation: Option<String>,
    pub criteria: Vec<CriterionResult>,
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Repository-relative form of `path`, which must name a location inside the checkout.
fn relative(path: &str, checkout: &Path) -> Option<String> {
    let path = Path::new(path);
    let relative = if path.is_absolute() { path.strip_prefix(checkout).ok()? } else { path };
    if relative.as_os_str().is_empty()
        || !relative.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let text = relative.to_str()?;
    (!text.starts_with(".git/")).then(|| text.to_owned())
}

fn priority(finding: &Finding) -> Option<u8> {
    if let Some(value) = finding.priority {
        return u8::try_from(value).ok().filter(|p| *p <= 3);
    }
    let tag = finding.title.trim_start().strip_prefix("[P")?;
    let digit = tag.chars().next()?.to_digit(10)?;
    (digit <= 3 && tag[1..].starts_with(']')).then_some(digit as u8)
}

pub(super) fn decide(input: Input<'_>) -> Decision {
    let mut decision = Decision::default();
    let parsed = input
        .output
        .filter(|text| text.len() <= MAX_OUTPUT)
        .and_then(|text| serde_json::from_str::<Output>(text).ok());
    let Some(output) = parsed else {
        decision.verdict = Some(ReviewVerdict::Invalid);
        decision.reasons.push(
            "the reviewer's output is missing, too large, or does not match the review schema"
                .into(),
        );
        return decision;
    };
    if input.checkout_changed {
        decision.verdict = Some(ReviewVerdict::Invalid);
        decision.reasons.push(
            "the reviewer changed its private checkout; a read-only review cannot be trusted"
                .into(),
        );
        return decision;
    }
    let correct = match output.overall_correctness.as_str() {
        "patch is correct" => true,
        "patch is incorrect" => false,
        _ => {
            decision.verdict = Some(ReviewVerdict::Invalid);
            decision
                .reasons
                .push("the reviewer's overall verdict is not one of the two allowed".into());
            return decision;
        },
    };
    decision.overall_correct = Some(correct);
    decision.overall_explanation = Some(clip(&output.overall_explanation, 2000));
    if let Some(expected) = input.criteria {
        match check_criteria(expected, output.criteria.as_deref().unwrap_or_default()) {
            Ok(results) => decision.criteria = results,
            Err(reason) => {
                decision.verdict = Some(ReviewVerdict::Invalid);
                decision.reasons.push(reason);
                return decision;
            },
        }
    }
    let ids = |status| {
        decision
            .criteria
            .iter()
            .filter(|c| c.status == status)
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (unmet, unknown) = (ids(CriterionStatus::Unmet), ids(CriterionStatus::Unknown));
    for (index, finding) in output.findings.iter().enumerate() {
        let title = clip(&finding.title, 200);
        let drop = |reason: &str| DroppedFinding { title: title.clone(), reason: reason.into() };
        if index >= MAX_FINDINGS {
            decision.dropped.push(drop("more than 50 findings; the rest were not considered"));
            continue;
        }
        let Some(path) = relative(&finding.code_location.absolute_file_path, input.checkout) else {
            decision.dropped.push(drop("its location is outside the reviewed repository"));
            continue;
        };
        if !(input.exists)(&path) {
            decision.dropped.push(drop("its file does not exist in the reviewed commit"));
            continue;
        }
        let (start, end) =
            (finding.code_location.line_range.start, finding.code_location.line_range.end);
        let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
            decision.dropped.push(drop("its line range is invalid"));
            continue;
        };
        if start == 0 || end < start {
            decision.dropped.push(drop("its line range is invalid"));
            continue;
        }
        match input.changes.0.get(&path) {
            None => {
                decision.dropped.push(drop("its file is not part of this change"));
                continue;
            },
            Some(Some(ranges))
                if !ranges.iter().any(|(first, last)| {
                    start <= last.saturating_add(TOLERANCE)
                        && end.saturating_add(TOLERANCE) >= *first
                }) =>
            {
                decision.dropped.push(drop("its lines are not part of this change"));
                continue;
            },
            Some(_) => (),
        }
        let priority = priority(finding);
        let confidence = if finding.confidence_score.is_finite() {
            finding.confidence_score.clamp(0.0, 1.0)
        } else {
            0.0
        };
        decision.findings.push(ReviewFinding {
            title,
            body: clip(&finding.body, 2000),
            priority,
            confidence,
            path,
            start,
            end,
            blocking: priority.is_some_and(|p| p <= 1) && confidence >= MIN_BLOCKING_CONFIDENCE,
        });
    }
    if !decision.dropped.is_empty() {
        decision.reasons.push(format!(
            "{} finding(s) were dropped as not about this change",
            decision.dropped.len()
        ));
    }
    let blocking = decision.findings.iter().filter(|f| f.blocking).count();
    if blocking > 0 || !correct || !unmet.is_empty() {
        if !unmet.is_empty() {
            decision.reasons.push(format!("acceptance criteria not met: {unmet}"));
        }
        if blocking > 0 {
            decision.reasons.push(format!("{blocking} blocking finding(s) in the change"));
        }
        if !correct {
            decision.reasons.push("the reviewer judged the patch incorrect".into());
        }
        decision.verdict = Some(ReviewVerdict::NeedsFix);
        return decision;
    }
    let mut blocked = Vec::new();
    if !unknown.is_empty() {
        blocked.push(format!("acceptance criteria the reviewer could not confirm: {unknown}"));
    }
    match &input.verification {
        Verification::Passed => (),
        Verification::Missing => {
            blocked.push("no passing verification is bound to this review".to_owned())
        },
        Verification::NotPassing(why) => blocked.push(format!("the bound verification {why}")),
    }
    if input.diff_truncated {
        blocked.push("the change was too large to show the reviewer in full".into());
    }
    if blocked.is_empty() {
        decision.verdict = Some(ReviewVerdict::Approved);
    } else {
        decision.reasons.extend(blocked);
        decision.verdict = Some(ReviewVerdict::Blocked);
    }
    decision
}

/// One result per expected criterion, in the task's order, or why the reviewer's are unusable.
fn check_criteria(
    expected: &[Criterion],
    given: &[CriterionOutput],
) -> Result<Vec<CriterionResult>, String> {
    let mut seen = BTreeMap::new();
    for result in given {
        if !expected.iter().any(|c| c.id == result.id) {
            return Err(format!(
                "the reviewer judged an unexpected criterion {}",
                clip(&result.id, 20)
            ));
        }
        if seen.insert(result.id.as_str(), result).is_some() {
            return Err(format!("the reviewer judged criterion {} more than once", result.id));
        }
    }
    let missing: Vec<&str> = expected
        .iter()
        .filter(|c| !seen.contains_key(c.id.as_str()))
        .map(|c| c.id.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!("the reviewer did not judge criteria {}", missing.join(", ")));
    }
    Ok(expected
        .iter()
        .map(|criterion| {
            let result = seen[criterion.id.as_str()];
            CriterionResult {
                id: criterion.id.clone(),
                status: result.status,
                evidence: clip(&result.evidence, 500),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -2 \
                        +2 @@ fn a()\n-old\n+new\n@@ -10,0 +11,3 @@\n+x\n+y\n+z\ndiff --git \
                        a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 \
                        @@\n-a\n-b\ndiff --git a/src/del.rs b/src/del.rs\n--- a/src/del.rs\n+++ \
                        b/src/del.rs\n@@ -5,2 +4,0 @@\n-a\n-b\n";

    fn changes() -> Changes {
        Changes::from_unified_zero(DIFF)
    }

    fn finding(
        path: &str,
        start: i64,
        end: i64,
        priority: i64,
        confidence: f64,
    ) -> serde_json::Value {
        json!({"title": format!("[P{priority}] t"), "body": "b", "confidence_score": confidence,
               "priority": priority,
               "code_location": {"absolute_file_path": path,
                                 "line_range": {"start": start, "end": end}}})
    }

    fn output(findings: Vec<serde_json::Value>, correct: bool) -> String {
        json!({"findings": findings,
               "overall_correctness": if correct { "patch is correct" } else { "patch is incorrect" },
               "overall_explanation": "e", "overall_confidence_score": 0.5})
        .to_string()
    }

    fn decide_with(output: Option<&str>, verification: Verification) -> Decision {
        let changes = changes();
        decide(Input {
            output,
            checkout_changed: false,
            checkout: Path::new("/co"),
            changes: &changes,
            exists: &|path| path != "missing.rs",
            verification,
            diff_truncated: false,
            criteria: None,
        })
    }

    #[test]
    fn unified_zero_hunks_map_to_new_side_ranges() {
        let changes = changes();
        assert_eq!(changes.0["src/a.rs"], Some(vec![(2, 2), (11, 13)]));
        assert_eq!(changes.0["src/del.rs"], Some(vec![(4, 4)]));
        assert!(!changes.0.contains_key("gone.rs"), "deleted files have no new side");
    }

    #[test]
    fn approval_needs_a_correct_patch_no_blocking_finding_and_a_passing_check() {
        let clean = output(vec![], true);
        assert_eq!(
            decide_with(Some(&clean), Verification::Passed).verdict,
            Some(ReviewVerdict::Approved)
        );
        for verification in [Verification::Missing, Verification::NotPassing("failed".into())] {
            let decision = decide_with(Some(&clean), verification);
            assert_eq!(decision.verdict, Some(ReviewVerdict::Blocked));
            assert!(decision.reasons.iter().any(|r| r.contains("verification")));
        }
        let incorrect = output(vec![], false);
        assert_eq!(
            decide_with(Some(&incorrect), Verification::Passed).verdict,
            Some(ReviewVerdict::NeedsFix)
        );
        let blocking = output(vec![finding("/co/src/a.rs", 12, 12, 1, 0.9)], true);
        let decision = decide_with(Some(&blocking), Verification::Passed);
        assert_eq!(decision.verdict, Some(ReviewVerdict::NeedsFix));
        assert!(decision.findings[0].blocking);
    }

    #[test]
    fn minor_or_unsure_findings_are_advisory() {
        let minor = output(
            vec![finding("src/a.rs", 2, 2, 2, 0.9), finding("src/a.rs", 2, 2, 0, 0.3)],
            true,
        );
        let decision = decide_with(Some(&minor), Verification::Passed);
        assert_eq!(decision.verdict, Some(ReviewVerdict::Approved));
        assert_eq!(decision.findings.len(), 2);
        assert!(decision.findings.iter().all(|f| !f.blocking));
    }

    #[test]
    fn findings_not_about_the_change_are_dropped_with_reasons() {
        let stray = output(
            vec![
                finding("/etc/passwd", 1, 1, 0, 1.0),
                finding("/co/../etc/passwd", 1, 1, 0, 1.0),
                finding("missing.rs", 1, 1, 0, 1.0),
                finding("src/a.rs", 40, 41, 0, 1.0),
                finding("src/other.rs", 1, 1, 0, 1.0),
                finding("src/a.rs", 0, 1, 0, 1.0),
                finding("src/a.rs", 5, 4, 0, 1.0),
                finding("src/a.rs", 14, 16, 0, 1.0),
            ],
            true,
        );
        let decision = decide_with(Some(&stray), Verification::Passed);
        assert_eq!(decision.dropped.len(), 7, "{:?}", decision.dropped);
        assert_eq!(decision.findings.len(), 1, "within tolerance of 11..=13");
        assert_eq!(decision.verdict, Some(ReviewVerdict::NeedsFix));
    }

    #[test]
    fn untrustworthy_output_is_invalid_never_approved() {
        for text in [None, Some("not json"), Some(r#"{"findings":[]}"#)] {
            assert_eq!(
                decide_with(text, Verification::Passed).verdict,
                Some(ReviewVerdict::Invalid)
            );
        }
        let odd = output(vec![], true).replace("patch is correct", "looks fine");
        assert_eq!(
            decide_with(Some(&odd), Verification::Passed).verdict,
            Some(ReviewVerdict::Invalid)
        );
        let changes = changes();
        let clean = output(vec![], true);
        let tampered = decide(Input {
            output: Some(&clean),
            checkout_changed: true,
            checkout: Path::new("/co"),
            changes: &changes,
            exists: &|_| true,
            verification: Verification::Passed,
            diff_truncated: false,
            criteria: None,
        });
        assert_eq!(tampered.verdict, Some(ReviewVerdict::Invalid));
    }

    #[test]
    fn a_truncated_diff_cannot_be_approved_and_paths_alone_still_filter() {
        let changes = Changes::paths_only(["src/a.rs".to_owned()]);
        let text = output(
            vec![finding("src/a.rs", 900, 901, 3, 0.9), finding("b.rs", 1, 1, 3, 0.9)],
            true,
        );
        let decision = decide(Input {
            output: Some(&text),
            checkout_changed: false,
            checkout: Path::new("/co"),
            changes: &changes,
            exists: &|_| true,
            verification: Verification::Passed,
            diff_truncated: true,
            criteria: None,
        });
        assert_eq!(decision.verdict, Some(ReviewVerdict::Blocked));
        assert_eq!((decision.findings.len(), decision.dropped.len()), (1, 1));
    }

    #[test]
    fn priority_comes_from_the_field_or_the_title_tag() {
        let tagged: Finding = serde_json::from_value(json!({
            "title": "[P1] x", "body": "", "confidence_score": 1.0, "priority": null,
            "code_location": {"absolute_file_path": "a", "line_range": {"start": 1, "end": 1}}}))
        .unwrap();
        assert_eq!(priority(&tagged), Some(1));
        let wild: Finding = serde_json::from_value(json!({
            "title": "x", "body": "", "confidence_score": 1.0, "priority": 9,
            "code_location": {"absolute_file_path": "a", "line_range": {"start": 1, "end": 1}}}))
        .unwrap();
        assert_eq!(priority(&wild), None);
    }

    fn criteria() -> Vec<Criterion> {
        ["AC1", "AC2"]
            .iter()
            .map(|id| Criterion { id: (*id).into(), text: "t".into(), hash: "h".into() })
            .collect()
    }

    fn with_criteria(results: serde_json::Value, verification: Verification) -> Decision {
        let changes = changes();
        let mut text: serde_json::Value = serde_json::from_str(&output(vec![], true)).unwrap();
        text["criteria"] = results;
        let text = text.to_string();
        let expected = criteria();
        decide(Input {
            output: Some(&text),
            checkout_changed: false,
            checkout: Path::new("/co"),
            changes: &changes,
            exists: &|_| true,
            verification,
            diff_truncated: false,
            criteria: Some(&expected),
        })
    }

    #[test]
    fn each_criterion_must_be_judged_exactly_once_and_all_met_to_approve() {
        let r = |pairs: &[(&str, &str)]| -> serde_json::Value {
            pairs.iter().map(|(id, s)| json!({"id": id, "status": s, "evidence": "e"})).collect()
        };
        let met = with_criteria(r(&[("AC2", "met"), ("AC1", "met")]), Verification::Passed);
        assert_eq!(met.verdict, Some(ReviewVerdict::Approved));
        assert_eq!(met.criteria.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["AC1", "AC2"]);
        let unmet = with_criteria(r(&[("AC1", "unmet"), ("AC2", "met")]), Verification::Passed);
        assert_eq!(unmet.verdict, Some(ReviewVerdict::NeedsFix));
        let unknown = with_criteria(r(&[("AC1", "met"), ("AC2", "unknown")]), Verification::Passed);
        assert_eq!(unknown.verdict, Some(ReviewVerdict::Blocked));
        for broken in [
            r(&[("AC1", "met")]),
            r(&[("AC1", "met"), ("AC2", "met"), ("AC3", "met")]),
            r(&[("AC1", "met"), ("AC1", "met"), ("AC2", "met")]),
            json!(null),
        ] {
            assert_eq!(
                with_criteria(broken, Verification::Passed).verdict,
                Some(ReviewVerdict::Invalid)
            );
        }
        let failing = with_criteria(
            r(&[("AC1", "met"), ("AC2", "met")]),
            Verification::NotPassing("failed".into()),
        );
        assert_eq!(failing.verdict, Some(ReviewVerdict::Blocked));
    }

    #[test]
    fn task_reviews_require_a_result_per_criterion() {
        assert!(!schema(false)["required"].to_string().contains("criteria"));
        assert!(schema(true)["required"].to_string().contains("criteria"));
    }
}
