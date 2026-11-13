//! `compute().llms().list()` against the body the gateway really sends
//! (captured from staging, `GET /@{projectSlug}/compute/llms`).

use k9o::envelope::Response;
use k9o::LlmModel;

const FIXTURE: &str = include_str!("fixtures/compute_llms_list.json");

#[test]
fn llm_list_deserializes_the_gateway_body() {
    let res: Response<Vec<LlmModel>> = serde_json::from_str(FIXTURE).expect("gateway body");
    assert_eq!(res.data.len(), 10);

    let phi = &res.data[0];
    assert_eq!(phi.id, "phi-3-mini");
    assert_eq!(phi.provider, "self_hosted");
    assert_eq!(phi.provider_class, "platform");
    assert_eq!(phi.modality, "text_completion");
    assert_eq!(phi.min_tier, "BASE_CAMP");
    assert!(phi.invocable);
    assert_eq!(phi.price_in, Some(0.18));
    assert_eq!(phi.price_out, Some(0.18));
    assert_eq!(phi.price_unit.as_deref(), Some("usd_per_million_tokens"));
    assert_eq!(phi.reason, None);

    let unpriced = &res.data[2];
    assert_eq!(unpriced.id, "mistral-8x7b");
    assert_eq!(unpriced.price_in, None);

    let tenant = res.data.iter().find(|m| m.id == "gpt-4o").unwrap();
    assert!(!tenant.invocable);
    assert_eq!(tenant.reason.as_deref(), Some("tenant_key_only"));
}
