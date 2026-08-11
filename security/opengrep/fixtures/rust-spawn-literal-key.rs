// Negative fixture: if-let binds gateway_creds but reinjects a literal
// GW_API_KEY that does not derive from $C — must fire the creds rule.
fn compose_governed_literal_key(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
    if let Some(creds) = gateway_creds {
        upsert_env(&mut env, "GW_API_KEY", "literal-not-from-creds");
    }
}
