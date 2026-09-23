use super::*;
use serde_json::json;

fn request() -> SystemOneRequest {
    serde_json::from_value(json!({
        "model":"jev-latest", "state":{"message":"Refund please"},
        "questions":{
            "billing":{"type":"noul"},
            "route":{"type":"choice","criteria":{"billing":null,"other":"Other requests"}},
            "urgency":{"type":"score","criteria":["low",{"label":"high"}]}
        }
    }))
    .unwrap()
}

fn response() -> serde_json::Value {
    json!({
        "model":"jev-1.13.0",
        "answers":{
            "billing":{"type":"noul","noul":0.9},
            "route":{"type":"choice","choice":"billing","confidence":0.9,
                "probabilities":{"billing":0.9,"other":0.1}},
            "urgency":{"type":"score","score":0.6,"confidence":0.8,
                "probabilities":{"0":0.4,"1":0.6},"legend":{"0":"low","1":{"label":"high"}}}
        },
        "usage":{"input_tokens":120,"output_tokens":3}
    })
}

#[test]
fn mixed_questions_preserve_exact_response_bytes_and_extensions() {
    let req = request();
    req.validate().unwrap();
    let mut value = response();
    value["future_field"] = json!({"keep":true});
    let raw = serde_json::to_vec_pretty(&value).unwrap();
    let parsed = SystemOneResponseWithBytes::parse(raw.clone(), &req).unwrap();
    assert_eq!(parsed.raw_bytes, raw);
    assert!(parsed.response.id.is_none());
    assert!(parsed.provider_signature_id().is_err());
}

#[test]
fn rejects_invalid_question_shapes_and_bounds() {
    for patch in [
        json!({"type":"unknown"}),
        json!({"type":"choice","criteria":{}}),
        json!({"type":"score","criteria":[]}),
        json!({"type":"score","criteria":vec!["level";11]}),
        json!({"type":"noul","extra":"ignored?"}),
    ] {
        let value = json!({"model":"jev","state":"content","questions":{"q":patch}});
        assert!(serde_json::from_value::<SystemOneRequest>(value)
            .map(|r| r.validate().is_err())
            .unwrap_or(true));
    }
    for state in [json!(null), json!(12), json!(true)] {
        assert!(serde_json::from_value::<SystemOneRequest>(json!({
            "model":"jev","state":state,"questions":{"q":{"type":"noul"}}
        }))
        .is_err());
    }
}

#[test]
fn rejects_unbillable_or_mismatched_provider_results() {
    let cases = [
        ("/usage/input_tokens", json!(-1)),
        ("/usage/output_tokens", json!(i32::MAX)),
        ("/answers/billing/noul", json!(1.1)),
        ("/answers/route/choice", json!("unknown")),
        ("/answers/urgency/score", json!(2)),
        ("/answers", json!({})),
    ];
    for (path, replacement) in cases {
        let mut value = response();
        *value.pointer_mut(path).unwrap() = replacement;
        assert!(
            SystemOneResponseWithBytes::parse(serde_json::to_vec(&value).unwrap(), &request())
                .is_err(),
            "{path}"
        );
    }
}

#[test]
fn tee_ids_are_safe_header_values_and_path_segments() {
    for id in ["", "../signature", "a/b", "q?x=y", "id\r\ninjected:yes"] {
        let mut value = response();
        value["id"] = json!(id);
        let parsed =
            SystemOneResponseWithBytes::parse(serde_json::to_vec(&value).unwrap(), &request())
                .unwrap();
        assert!(parsed.provider_signature_id().is_err());
    }
    let mut value = response();
    value["id"] = json!("decision-123_abc");
    let parsed =
        SystemOneResponseWithBytes::parse(serde_json::to_vec(&value).unwrap(), &request()).unwrap();
    assert_eq!(parsed.provider_signature_id().unwrap(), "decision-123_abc");
}
