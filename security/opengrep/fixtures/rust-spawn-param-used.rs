// Positive fixture: gateway_creds parameter is consumed after inject.
fn compose_governed_param_used(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
    if let Some(creds) = gateway_creds {
        upsert_env(&mut env, "GW_API_KEY", &creds.api_key);
    }
}
