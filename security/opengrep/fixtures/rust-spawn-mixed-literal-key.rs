// Negative fixture: GATEWAY_URL derives from $C but GW_API_KEY is a literal.
// A URL-only exemption must not silence the governed-creds rule.
fn compose_governed_mixed_literal_key(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
    if let Some(creds) = gateway_creds {
        upsert_env(&mut env, "GW_API_KEY", "literal-not-from-creds");
        let u = creds.gateway_url.trim();
        upsert_env(&mut env, "GATEWAY_URL", u);
    }
}
