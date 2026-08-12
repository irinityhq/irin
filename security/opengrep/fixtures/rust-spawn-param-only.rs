// Negative fixture (PR #19): gateway_creds parameter spelling without use.
fn compose_governed_param_only(
    mut env: Vec<(String, String)>,
    gateway_creds: Option<GatewayChildCredentials>,
) {
    upsert_env(&mut env, "COUNCIL_VIA_GATEWAY", "1");
}
