-- ==========================================================================
-- config.lua — Load model registry + API keys at init.
-- Called once per worker via init_by_lua_block.
-- ==========================================================================

local cjson = require "cjson.safe"
local _M = {}

-- Module-level state (survives across requests within a worker)
_M.providers = {}
_M.models    = {}
_M.aliases   = {}
_M.api_keys  = {}

function _M.init()
    -- Read API keys from environment (only available in init phase)
    _M.api_keys = {
        xai          = os.getenv("XAI_API_KEY") or "",
        openai       = os.getenv("OPENAI_API_KEY") or "",
        anthropic    = os.getenv("ANTHROPIC_API_KEY") or "",
        nvidia       = os.getenv("NVIDIA_API_KEY") or "",
        vertex       = os.getenv("VERTEX_ADC_TOKEN") or "",
        -- claude-cli uses host-side proxy (no real key needed)
        ["claude-cli"] = "cli-proxy",
        -- gpt-cli mirrors claude-cli: host-side codex-proxy.py wraps `codex
        -- exec` (ChatGPT Pro/Plus OAuth). No real OPENAI_API_KEY needed.
        ["gpt-cli"]    = "cli-proxy",
        -- Legacy/scout Gemini proxy. Active Gemini aliases use Vertex ADC;
        -- keep this key only for explicit proxy experiments while Gemini CLI
        -- retires toward the Antigravity terminal (`agy`).
        ["gemini-cli"] = "cli-proxy",
        chaos          = os.getenv("_CHAOS_DUMMY") or "test",
        -- Phase 0.5: council uses a service-identity token, not a per-user
        -- bearer. The router's §5.2a custom-header branch picks this up via
        -- provider.auth_header == "X-Gateway-Auth" + auth_prefix == "".
        council        = os.getenv("COUNCIL_GATEWAY_TOKEN") or "",
    }

    -- Load models.json
    local conf_path = "/usr/local/openresty/nginx/conf/conf/models.json"
    local f, err = io.open(conf_path, "r")
    if not f then
        ngx.log(ngx.ERR, "config: failed to open models.json: ", err)
        return
    end
    local raw = f:read("*a")
    f:close()

    local data, decode_err = cjson.decode(raw)
    if not data then
        ngx.log(ngx.ERR, "config: failed to parse models.json: ", decode_err)
        return
    end

    _M.providers = data.providers or {}
    _M.models    = data.models or {}
    _M.aliases   = data.aliases or {}

    -- Load shape limits
    local shape_path = "/usr/local/openresty/nginx/conf/conf/shape_limits.json"
    local f_shape, err_shape = io.open(shape_path, "r")
    if f_shape then
        local raw_shape = f_shape:read("*a")
        f_shape:close()
        local shape_data = cjson.decode(raw_shape)
        if shape_data then
            _M.shape_limits = shape_data
        else
            ngx.log(ngx.ERR, "config: failed to parse shape_limits.json")
            _M.shape_limits = { default = {} }
        end
    else
        ngx.log(ngx.WARN, "config: shape_limits.json not found, using empty defaults")
        _M.shape_limits = { default = {} }
    end

    -- Resolve base_url_env overrides (e.g. CLAUDE_PROXY_URL)
    for name, prov in pairs(_M.providers) do
        if prov.base_url_env then
            local env_val = os.getenv(prov.base_url_env)
            if env_val and env_val ~= "" then
                ngx.log(ngx.INFO, "config: ", name, " base_url overridden via ",
                        prov.base_url_env, " → ", env_val)
                prov.base_url = env_val
            end
        end
    end

    local model_count = 0
    for _ in pairs(_M.models) do model_count = model_count + 1 end

    local key_count = 0
    for _, v in pairs(_M.api_keys) do
        if v ~= "" then key_count = key_count + 1 end
    end

    ngx.log(ngx.INFO, "config: loaded ", model_count, " models, ",
            key_count, " API keys configured")
end

--- Resolve a model name (handles aliases).
-- @param name string — model name or alias
-- @return table|nil model config, string resolved model name
function _M.resolve_model(name)
    if not name or name == "" then
        return nil, ""
    end
    -- Check alias first
    local resolved = _M.aliases[name] or name
    local model = _M.models[resolved]
    return model, resolved
end

--- Get a model config by exact ID (no alias resolution).
-- @param model_id string — exact model identifier
-- @return table|nil model config
function _M.get_model(model_id)
    if not model_id then return nil end
    return _M.models[model_id]
end

--- Get the provider config for a model.
-- @param model_cfg table — model config from resolve_model
-- @return table|nil provider config
function _M.get_provider(model_cfg)
    if not model_cfg or not model_cfg.provider then
        return nil
    end
    return _M.providers[model_cfg.provider]
end

--- Get the API key for a provider.
-- @param provider_name string
-- @return string API key (may be empty)
function _M.get_api_key(provider_name)
    return _M.api_keys[provider_name] or ""
end

return _M
