use k9o::KError;

#[test]
fn test_kerror_constructs() {
    let e = KError::new("K9O_INVALID_FIELD_VALUE", "bad");
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert_eq!(e.message, "bad");
    assert!(e.context.is_empty());
    assert!(e.recommendation.is_none());
    assert!(e.status.is_none());
}

#[test]
fn test_kerror_with_recommendation() {
    let e = KError::new("X", "y").with_recommendation("do this");
    assert_eq!(e.recommendation.as_deref(), Some("do this"));
}

#[test]
fn test_kerror_with_context() {
    let e = KError::new("X", "y")
        .with_context("dim", "mau")
        .with_context("limit", 1000);
    assert_eq!(e.context.get("dim").unwrap().as_str(), Some("mau"));
    assert_eq!(e.context.get("limit").unwrap().as_i64(), Some(1000));
}

#[test]
fn test_kerror_display() {
    let e = KError::new("K9O_TIER_CAP_HIT", "cap");
    let msg = format!("{}", e);
    assert!(msg.contains("K9O_TIER_CAP_HIT"));
    assert!(msg.contains("cap"));
}

#[test]
fn test_kerror_is_error() {
    fn requires_error<E: std::error::Error>(_: &E) {}
    let e = KError::new("X", "y");
    requires_error(&e);
}
