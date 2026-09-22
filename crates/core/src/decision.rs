//! Decision primitive — a Jev/System One-compatible typed-decision client.
//!
//! Wraps `POST {base}/systemone`, the TypeSafe AI "Jev" contract that
//! `ai.fpl.dev/v1` hosts a compatible (and extended) implementation of: one
//! endpoint, a `state` plus a map of `questions`, and three bounded question
//! kinds — `choice` (pick one of a known set), `noul` (a calibrated yes/no
//! probability), `score` (a position on an ordered rubric). Every answer
//! carries a confidence/probability, never free text.
//!
//! This exists to formalize a pattern already scattered through the pipeline
//! as ad-hoc gates (`verified_by_human`, the §9.6 advisory-substitute check,
//! the §6.8 layout escape hatch): make a bounded call automatically, and only
//! automatically, when it clears a confidence bar — otherwise surface it.
//! Per DESIGN.md §3.4/§7.9's "curated, not generated" stance, `Question`
//! kinds are deliberately bounded (an enum of known options, a probability, a
//! rubric position) — never a request for free-form/open-ended output.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_BASE_URL: &str = "https://ai.fpl.dev/v1";
const DEFAULT_MODEL: &str = "jev-latest";

/// Errors from a decision call.
#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("SYSTEMONE_API_KEY is not set (put it in .env)")]
    MissingKey,
    #[error("System One API error: {0}")]
    Api(String),
    #[error("System One request failed: {0}")]
    Http(String),
    #[error("System One response missing or malformed answer for question '{0}'")]
    MalformedAnswer(String),
}

/// A bounded question sent to System One. Deliberately has no "free text"
/// variant — every kind resolves to a typed, confidence-scored answer.
#[derive(Debug, Clone)]
enum Question {
    /// Select one option from a defined set (up to 255 options).
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    /// Yes/no as a calibrated probability; the number itself is the belief.
    Noul {
        instructions: String,
        true_desc: String,
        false_desc: String,
    },
    /// Rate along an ordered rubric (2-10 levels, low to high).
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

/// Answer to a [`Question::Choice`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

/// Answer to a [`Question::Score`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreAnswer {
    pub score: f64,
    pub confidence: f64,
}

/// A typed answer as returned from [`DecisionClient::ask_many`], where the
/// caller doesn't know each key's question kind at the call site the way
/// `ask_choice`/`ask_noul`/`ask_score` do individually.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice(ChoiceAnswer),
    Noul(f64),
    Score(ScoreAnswer),
}

/// One named question for a batched [`DecisionClient::ask_many`] call — pairs
/// a bounded question with the key its answer comes back under.
///
/// **Batching independent questions** (no dependency between them) is a pure
/// win: send all of them in one call instead of one round trip each — the
/// Jev/System One contract already accepts a multi-key `questions` map, this
/// type is what was missing to use it.
///
/// **Speculative branch pre-fetch**, for a genuinely *dependent* chain (Q2's
/// real content differs depending on which option Q1 resolves to): include
/// Q1 plus every possible Q2 variant in the same `ask_many` call, then keep
/// only the [`Answer`] for whichever variant matches Q1's actual answer and
/// discard the rest. Trades wasted compute (the discarded branches) for one
/// round trip instead of two — worth it at shallow depth / small branching
/// factor (it grows as branching_factor^depth), not for a long linear chain.
/// No code here builds the branch-selection logic yet because nothing in
/// this crate has a real dependent decision tree today (the curated topology
/// set is one entry) — this doc is the design for whenever one exists,
/// wiring it in then rather than speculatively now.
#[derive(Debug, Clone)]
pub struct NamedQuestion {
    key: String,
    question: Question,
}

impl NamedQuestion {
    pub fn choice(
        key: impl Into<String>,
        instructions: impl Into<String>,
        options: &[(&str, &str)],
    ) -> Self {
        NamedQuestion {
            key: key.into(),
            question: Question::Choice {
                instructions: instructions.into(),
                options: options
                    .iter()
                    .map(|(name, desc)| (name.to_string(), desc.to_string()))
                    .collect(),
            },
        }
    }

    pub fn noul(
        key: impl Into<String>,
        instructions: impl Into<String>,
        true_desc: impl Into<String>,
        false_desc: impl Into<String>,
    ) -> Self {
        NamedQuestion {
            key: key.into(),
            question: Question::Noul {
                instructions: instructions.into(),
                true_desc: true_desc.into(),
                false_desc: false_desc.into(),
            },
        }
    }

    pub fn score(key: impl Into<String>, instructions: impl Into<String>, levels: &[&str]) -> Self {
        NamedQuestion {
            key: key.into(),
            question: Question::Score {
                instructions: instructions.into(),
                levels: levels.iter().map(|s| s.to_string()).collect(),
            },
        }
    }
}

/// One recorded decision, kept for audit and eval scoring — the PCBBench
/// harness (a separate, EDA-agnostic repo) reads a serialized trace of these
/// to score decision confidence as an objective rubric criterion, not just a
/// black-box result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionRecord {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub chosen: String,
    pub confidence: f64,
    pub timestamp_unix: u64,
}

/// A System One / Jev-compatible client. Every successful call appends to
/// `trace`, so a caller can serialize the full decision history for a run.
#[derive(Debug, Clone)]
pub struct DecisionClient {
    base_url: String,
    api_key: String,
    model: String,
    pub trace: Vec<DecisionRecord>,
}

impl DecisionClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        DecisionClient {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            trace: Vec::new(),
        }
    }

    /// Build from `SYSTEMONE_API_KEY` in the environment (`.env`-loadable,
    /// same convention as `MOUSER_API_KEY`/JLCPCB creds). `SYSTEMONE_BASE_URL`
    /// optionally points at a different Jev-compatible endpoint;
    /// `SYSTEMONE_MODEL` optionally names a different model on that endpoint
    /// (`jev-latest` is the stock Jev API's own name, not universal — a
    /// gateway fronting multiple providers, like `ai.fpl.dev`'s bifrost, uses
    /// its own routable model ids instead, e.g. `convaiinnovations/laya` or
    /// `vercel/typesafe-ai/jev`; checked by hitting the real endpoint's
    /// `/v1/models`, not assumed).
    pub fn from_env() -> Result<Self, DecisionError> {
        let api_key = match std::env::var("SYSTEMONE_API_KEY") {
            Ok(key) if !key.trim().is_empty() => key,
            _ => return Err(DecisionError::MissingKey),
        };
        let mut client = DecisionClient::new(api_key);
        if let Ok(url) = std::env::var("SYSTEMONE_BASE_URL") {
            if !url.trim().is_empty() {
                client.base_url = url;
            }
        }
        if let Ok(model) = std::env::var("SYSTEMONE_MODEL") {
            if !model.trim().is_empty() {
                client.model = model;
            }
        }
        Ok(client)
    }

    /// Ask a bounded choice: which of `options` (name, description) best fits
    /// `state`? `key` names the question (and labels its trace entry).
    pub fn ask_choice(
        &mut self,
        state: &str,
        key: &str,
        instructions: &str,
        options: &[(&str, &str)],
    ) -> Result<ChoiceAnswer, DecisionError> {
        let question = Question::Choice {
            instructions: instructions.to_string(),
            options: options
                .iter()
                .map(|(name, desc)| (name.to_string(), desc.to_string()))
                .collect(),
        };
        let raw = self.ask(state, key, &question)?;
        let answer = parse_choice(&raw, key)?;
        self.record(key, "choice", &answer.choice, answer.confidence);
        Ok(answer)
    }

    /// Ask a calibrated yes/no probability in `0.0..=1.0`.
    pub fn ask_noul(
        &mut self,
        state: &str,
        key: &str,
        instructions: &str,
        true_desc: &str,
        false_desc: &str,
    ) -> Result<f64, DecisionError> {
        let question = Question::Noul {
            instructions: instructions.to_string(),
            true_desc: true_desc.to_string(),
            false_desc: false_desc.to_string(),
        };
        let raw = self.ask(state, key, &question)?;
        let noul = parse_noul(&raw, key)?;
        self.record(key, "noul", &format!("{noul:.3}"), noul);
        Ok(noul)
    }

    /// Ask for a position on an ordered rubric (`levels`, low to high).
    pub fn ask_score(
        &mut self,
        state: &str,
        key: &str,
        instructions: &str,
        levels: &[&str],
    ) -> Result<ScoreAnswer, DecisionError> {
        let question = Question::Score {
            instructions: instructions.to_string(),
            levels: levels.iter().map(|s| s.to_string()).collect(),
        };
        let raw = self.ask(state, key, &question)?;
        let answer = parse_score(&raw, key)?;
        self.record(
            key,
            "score",
            &format!("{:.3}", answer.score),
            answer.confidence,
        );
        Ok(answer)
    }

    /// Ask every one of `questions` in a single HTTP call — see
    /// [`NamedQuestion`]'s docs for why this beats one call per question,
    /// independent or (with the right follow-up set) dependent alike.
    /// Returns a map keyed by each question's own key; an error from the
    /// call fails the whole batch (there's no partial-batch success), same
    /// as any of the single-question methods failing.
    pub fn ask_many(
        &mut self,
        state: &str,
        questions: Vec<NamedQuestion>,
    ) -> Result<HashMap<String, Answer>, DecisionError> {
        let mut questions_json = serde_json::Map::new();
        for nq in &questions {
            questions_json.insert(nq.key.clone(), question_to_json(&nq.question));
        }
        let body = serde_json::json!({
            "model": self.model,
            "state": state,
            "questions": Value::Object(questions_json),
        });
        let value = self.post(&body)?;
        let answers = parse_many_answers(&value, &questions)?;

        let mut results = HashMap::with_capacity(answers.len());
        for a in answers {
            self.record(&a.key, a.kind, &a.chosen, a.confidence);
            results.insert(a.key, a.answer);
        }
        Ok(results)
    }

    fn record(&mut self, key: &str, kind: &str, chosen: &str, confidence: f64) {
        let timestamp_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.trace.push(DecisionRecord {
            key: key.to_string(),
            kind: kind.to_string(),
            chosen: chosen.to_string(),
            confidence,
            timestamp_unix,
        });
    }

    /// Low-level: send one question under `key`, return its raw answer object.
    fn ask(&self, state: &str, key: &str, question: &Question) -> Result<Value, DecisionError> {
        let body = serde_json::json!({
            "model": self.model,
            "state": state,
            "questions": { key: question_to_json(question) },
        });
        let value = self.post(&body)?;
        extract_answer(&value, key)
    }

    /// POST `body` to `{base}/systemone`, retrying a rate limit (429,
    /// honoring a `Retry-After` header when the server sends one) or a
    /// transient failure (5xx, network-level error) with backoff. Neither
    /// is treated as "the request was bad" the way a real 4xx (invalid key,
    /// malformed question) is — those two are infra conditions, not a
    /// verdict on the request's content, so a design run hitting one isn't
    /// a design failure, just a delay (legion-of-bom-x74e's follow-up: a
    /// caller doing many decisions should never see a retryable hiccup
    /// surface as if it were a real answer or a real error).
    fn post(&self, body: &Value) -> Result<Value, DecisionError> {
        let url = format!("{}/systemone", self.base_url.trim_end_matches('/'));
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match ureq::post(&url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .send_json(body.clone())
            {
                Ok(response) => {
                    return response
                        .into_json()
                        .map_err(|e| DecisionError::Http(e.to_string()));
                }
                Err(ureq::Error::Status(429, response)) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DecisionError::Api(format!(
                            "rate limited (429) after {attempt} attempt(s), giving up"
                        )));
                    }
                    let delay = response
                        .header("Retry-After")
                        .and_then(|h| h.parse::<u64>().ok())
                        .unwrap_or_else(|| backoff_secs(attempt));
                    std::thread::sleep(std::time::Duration::from_secs(delay));
                }
                Err(ureq::Error::Status(code, _response)) if (500..600).contains(&code) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DecisionError::Http(format!(
                            "HTTP {code} after {attempt} attempt(s), giving up"
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(backoff_secs(attempt)));
                }
                Err(ureq::Error::Status(code, response)) => {
                    // Any other 4xx (bad request, invalid key, ...) is the
                    // request itself being wrong -- retrying just spends
                    // more of the rate-limit budget on the same failure.
                    let message = response
                        .into_string()
                        .ok()
                        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                        .and_then(|v| extract_error_message(&v))
                        .unwrap_or_else(|| format!("HTTP {code}"));
                    return Err(DecisionError::Api(message));
                }
                Err(ureq::Error::Transport(t)) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DecisionError::Http(t.to_string()));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(backoff_secs(attempt)));
                }
            }
        }
    }
}

/// Requests beyond this many attempts give up rather than retry forever —
/// covers a genuinely down endpoint or a rate limit that isn't recovering.
const MAX_ATTEMPTS: u32 = 5;

/// Backoff when the server doesn't say how long to wait (no `Retry-After`):
/// doubles each attempt, starting at 2s (2, 4, 8, 16s for attempts 1-4).
fn backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt)
}

fn question_to_json(question: &Question) -> Value {
    match question {
        Question::Choice {
            instructions,
            options,
        } => {
            let criteria: BTreeMap<&str, &str> = options
                .iter()
                .map(|(name, desc)| (name.as_str(), desc.as_str()))
                .collect();
            serde_json::json!({
                "type": "choice",
                "instructions": instructions,
                "criteria": criteria,
            })
        }
        Question::Noul {
            instructions,
            true_desc,
            false_desc,
        } => serde_json::json!({
            "type": "noul",
            "instructions": instructions,
            "criteria": { "true": true_desc, "false": false_desc },
        }),
        Question::Score {
            instructions,
            levels,
        } => serde_json::json!({
            "type": "score",
            "instructions": instructions,
            "criteria": levels,
        }),
    }
}

/// Parse a `choice` answer's raw JSON — shared by the single-question and
/// batched call paths so they can't drift on how a `choice` answer is read.
fn parse_choice(raw: &Value, key: &str) -> Result<ChoiceAnswer, DecisionError> {
    let choice = raw
        .get("choice")
        .and_then(Value::as_str)
        .ok_or_else(|| DecisionError::MalformedAnswer(key.to_string()))?
        .to_string();
    let confidence = raw.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    let probabilities = raw
        .get("probabilities")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default();
    Ok(ChoiceAnswer {
        choice,
        confidence,
        probabilities,
    })
}

/// Parse a `noul` answer's raw JSON.
fn parse_noul(raw: &Value, key: &str) -> Result<f64, DecisionError> {
    raw.get("noul")
        .and_then(Value::as_f64)
        .ok_or_else(|| DecisionError::MalformedAnswer(key.to_string()))
}

/// Parse a `score` answer's raw JSON.
fn parse_score(raw: &Value, key: &str) -> Result<ScoreAnswer, DecisionError> {
    let score = raw
        .get("score")
        .and_then(Value::as_f64)
        .ok_or_else(|| DecisionError::MalformedAnswer(key.to_string()))?;
    let confidence = raw.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(ScoreAnswer { score, confidence })
}

/// Parse every answer out of a multi-question System One response — the pure
/// core of [`DecisionClient::ask_many`], factored out so it's testable
/// without a live HTTP call. Returns `(key, kind, chosen-as-string,
/// confidence, Answer)` per question, in the same order as `questions`; the
/// caller (`ask_many`) turns each tuple into a trace record plus a result
/// map entry.
/// One question's parsed result from a batched call — a named tuple in
/// everything but syntax, so `ask_many` doesn't have to unpack a 5-element
/// tuple by position.
struct ParsedAnswer {
    key: String,
    kind: &'static str,
    chosen: String,
    confidence: f64,
    answer: Answer,
}

fn parse_many_answers(
    value: &Value,
    questions: &[NamedQuestion],
) -> Result<Vec<ParsedAnswer>, DecisionError> {
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(DecisionError::Api(err.to_string()));
    }
    questions
        .iter()
        .map(|nq| {
            let raw = value
                .get("answers")
                .and_then(|a| a.get(&nq.key))
                .cloned()
                .ok_or_else(|| DecisionError::MalformedAnswer(nq.key.clone()))?;
            let (kind, chosen, confidence, answer) = match &nq.question {
                Question::Choice { .. } => {
                    let a = parse_choice(&raw, &nq.key)?;
                    ("choice", a.choice.clone(), a.confidence, Answer::Choice(a))
                }
                Question::Noul { .. } => {
                    let v = parse_noul(&raw, &nq.key)?;
                    ("noul", format!("{v:.3}"), v, Answer::Noul(v))
                }
                Question::Score { .. } => {
                    let a = parse_score(&raw, &nq.key)?;
                    (
                        "score",
                        format!("{:.3}", a.score),
                        a.confidence,
                        Answer::Score(a),
                    )
                }
            };
            Ok(ParsedAnswer {
                key: nq.key.clone(),
                kind,
                chosen,
                confidence,
                answer,
            })
        })
        .collect()
}

/// Pull `answers[key]` out of a System One response, surfacing an
/// API-level `{"error": "..."}` body as [`DecisionError::Api`] rather than a
/// confusing "malformed answer".
fn extract_answer(value: &Value, key: &str) -> Result<Value, DecisionError> {
    if let Some(message) = extract_error_message(value) {
        return Err(DecisionError::Api(message));
    }
    value
        .get("answers")
        .and_then(|a| a.get(key))
        .cloned()
        .ok_or_else(|| DecisionError::MalformedAnswer(key.to_string()))
}

/// Read an API-level error message out of a response body. The real,
/// observed shape from `ai.fpl.dev/v1/systemone` is a nested object
/// (`{"error":{"message":"invalid api key","type":"invalid_request_error"}}`),
/// not the flat string this originally assumed — checked against the real
/// endpoint, not guessed. Accepts both so a spec change either way doesn't
/// silently stop being recognized as an error.
fn extract_error_message(value: &Value) -> Option<String> {
    let err = value.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_string());
    }
    err.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> DecisionClient {
        DecisionClient::new("test-key")
    }

    #[test]
    fn extracts_choice_answer() {
        let raw: Value = serde_json::from_str(
            r#"{"answers":{"topology":{"choice":"fuzz_face_silicon","confidence":0.92,
                "probabilities":{"fuzz_face_silicon":0.92,"tone_bender":0.08}}}}"#,
        )
        .unwrap();
        let answer = extract_answer(&raw, "topology").unwrap();
        assert_eq!(
            answer.get("choice").and_then(Value::as_str),
            Some("fuzz_face_silicon")
        );
        assert_eq!(answer.get("confidence").and_then(Value::as_f64), Some(0.92));
    }

    #[test]
    fn extracts_noul_answer() {
        let raw: Value =
            serde_json::from_str(r#"{"answers":{"is_critical_net":{"noul":0.95}}}"#).unwrap();
        let answer = extract_answer(&raw, "is_critical_net").unwrap();
        assert_eq!(answer.get("noul").and_then(Value::as_f64), Some(0.95));
    }

    #[test]
    fn extracts_score_answer() {
        let raw: Value =
            serde_json::from_str(r#"{"answers":{"dfm_severity":{"score":1.6,"confidence":0.94}}}"#)
                .unwrap();
        let answer = extract_answer(&raw, "dfm_severity").unwrap();
        assert_eq!(answer.get("score").and_then(Value::as_f64), Some(1.6));
        assert_eq!(answer.get("confidence").and_then(Value::as_f64), Some(0.94));
    }

    #[test]
    fn reports_api_errors() {
        let raw: Value = serde_json::from_str(r#"{"error":"invalid api key"}"#).unwrap();
        assert!(matches!(
            extract_answer(&raw, "x"),
            Err(DecisionError::Api(_))
        ));
    }

    /// The real shape `ai.fpl.dev/v1/systemone` returns for a 401 (checked
    /// against the live endpoint, not assumed) — `error` is an object with a
    /// `message`, not a flat string. `extract_answer`/`extract_error_message`
    /// must recognize both.
    #[test]
    fn reports_api_errors_with_the_real_nested_error_shape() {
        let raw: Value = serde_json::from_str(
            r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#,
        )
        .unwrap();
        match extract_answer(&raw, "x") {
            Err(DecisionError::Api(msg)) => assert_eq!(msg, "invalid api key"),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn backoff_doubles_each_attempt() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
    }

    #[test]
    fn missing_answer_is_malformed_not_panic() {
        let raw: Value = serde_json::from_str(r#"{"answers":{}}"#).unwrap();
        assert!(matches!(
            extract_answer(&raw, "missing"),
            Err(DecisionError::MalformedAnswer(k)) if k == "missing"
        ));
    }

    #[test]
    fn question_to_json_shapes_match_documented_contract() {
        let choice = Question::Choice {
            instructions: "pick a topology".into(),
            options: vec![("a".into(), "desc a".into()), ("b".into(), "desc b".into())],
        };
        let v = question_to_json(&choice);
        assert_eq!(v["type"], "choice");
        assert_eq!(v["criteria"]["a"], "desc a");

        let noul = Question::Noul {
            instructions: "is this net critical?".into(),
            true_desc: "yes".into(),
            false_desc: "no".into(),
        };
        let v = question_to_json(&noul);
        assert_eq!(v["type"], "noul");
        assert_eq!(v["criteria"]["true"], "yes");

        let score = Question::Score {
            instructions: "rate severity".into(),
            levels: vec!["low".into(), "medium".into(), "high".into()],
        };
        let v = question_to_json(&score);
        assert_eq!(v["type"], "score");
        assert_eq!(v["criteria"][0], "low");
    }

    #[test]
    fn from_env_reports_missing_key() {
        // SAFETY: test-only, single-threaded within this process's test run for
        // this var; no other test reads/writes SYSTEMONE_API_KEY.
        unsafe {
            std::env::remove_var("SYSTEMONE_API_KEY");
        }
        assert!(matches!(
            DecisionClient::from_env(),
            Err(DecisionError::MissingKey)
        ));
    }

    #[test]
    fn decision_trace_records_choice() {
        let mut c = client();
        c.trace.push(DecisionRecord {
            key: "enclosure_size".into(),
            kind: "choice".into(),
            chosen: "1590B".into(),
            confidence: 0.88,
            timestamp_unix: 0,
        });
        assert_eq!(c.trace.len(), 1);
        assert_eq!(c.trace[0].chosen, "1590B");
    }

    #[test]
    fn parses_a_batched_multi_question_response() {
        let questions = vec![
            NamedQuestion::choice("topology", "pick one", &[("a", "desc a"), ("b", "desc b")]),
            NamedQuestion::noul("tone_stack", "yes or no", "yes", "no"),
            NamedQuestion::score("gain_character", "rate it", &["tame", "unstable"]),
        ];
        let raw: Value = serde_json::from_str(
            r#"{"answers":{
                "topology":{"choice":"a","confidence":0.9,"probabilities":{"a":0.9,"b":0.1}},
                "tone_stack":{"noul":0.7},
                "gain_character":{"score":1.2,"confidence":0.6}
            }}"#,
        )
        .unwrap();
        let parsed = parse_many_answers(&raw, &questions).unwrap();
        assert_eq!(parsed.len(), 3);

        let topology = parsed.iter().find(|a| a.key == "topology").unwrap();
        assert_eq!(topology.key, "topology");
        assert_eq!(topology.kind, "choice");
        assert_eq!(topology.chosen, "a");
        assert_eq!(topology.confidence, 0.9);
        assert!(matches!(&topology.answer, Answer::Choice(a) if a.choice == "a"));

        let tone = parsed.iter().find(|a| a.key == "tone_stack").unwrap();
        assert!(matches!(&tone.answer, Answer::Noul(v) if (*v - 0.7).abs() < 1e-9));

        let gain = parsed.iter().find(|a| a.key == "gain_character").unwrap();
        assert!(matches!(&gain.answer, Answer::Score(a) if (a.score - 1.2).abs() < 1e-9));
    }

    #[test]
    fn batched_response_reports_api_error_before_per_question_parsing() {
        let questions = vec![NamedQuestion::noul("x", "?", "yes", "no")];
        let raw: Value = serde_json::from_str(r#"{"error":"invalid api key"}"#).unwrap();
        assert!(matches!(
            parse_many_answers(&raw, &questions),
            Err(DecisionError::Api(_))
        ));
    }

    #[test]
    fn batched_response_missing_one_answer_fails_that_key() {
        let questions = vec![
            NamedQuestion::noul("present", "?", "yes", "no"),
            NamedQuestion::noul("missing", "?", "yes", "no"),
        ];
        let raw: Value = serde_json::from_str(r#"{"answers":{"present":{"noul":0.5}}}"#).unwrap();
        assert!(matches!(
            parse_many_answers(&raw, &questions),
            Err(DecisionError::MalformedAnswer(k)) if k == "missing"
        ));
    }
}
