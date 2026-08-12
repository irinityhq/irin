// Negative fixture: derived GW_API_KEY + GATEWAY_URL pair followed by a
// literal GW_API_KEY overwrite. upsert_env replaces the prior value, so the
// final write is non-derived. The function-scoped requires-creds-param
// exemption alone is a false green; rejects-literal-gateway-env must fire.
fn compose_governed_derived_then_literal_overwrite(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
    if let Some(creds) = gateway_creds {
        upsert_env(&mut env, "GW_API_KEY", &creds.api_key);
        upsert_env(&mut env, "GATEWAY_URL", &creds.gateway_url);
    }
    // Final write wins — literal overwrites the derived key.
    upsert_env(&mut env, "GW_API_KEY", "literal-overwrite-after-derived");
}
