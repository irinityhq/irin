// Negative fixture: destructuring gateway_creds without reinjecting
// GW_API_KEY / GATEWAY_URL must still fire the governed-creds rule.
fn compose_governed_drop_only(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
    if let Some(creds) = gateway_creds {
        drop(creds);
    }
}
