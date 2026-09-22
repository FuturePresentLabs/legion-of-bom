-- JLCPCB (open API) client -- authoritative part data by LCSC component code.
-- Auth (reverse-engineered, verified live): each request is signed
-- HMAC-SHA256(secret, "METHOD\n{path}\n{timestamp}\n{nonce}\n{body}\n"),
-- base64-encoded, sent as
-- `Authorization: JOP appid=...,accesskey=...,timestamp=...,nonce=...,signature=...`.
--
-- Contract (crates/core/src/jlcpcb.rs):
--   component_by_code(req) -> component | nil
--     req = { app_id, access_key, secret_key, code }
--
-- `component` shape (matches JlcpcbComponent in jlcpcb.rs exactly; parameters
-- is an array of [name, value] pairs, Rust's `Vec<(String, String)>` wire
-- shape):
--   { component_code, component_model, package, description, datasheet_url,
--     library_type, stock, parameters = { { "Name", "Value" }, ... } }
--
-- Also exported, pure and host-I/O-free, so the string-to-sign format and
-- the response-parsing logic are each directly testable against a fixture
-- (no live API keys needed):
--   sign_for_test(req) -> string    req = { secret_key, method, path, timestamp, nonce, body }
--   parse_component_response(json)  -> component | nil
--
-- Host primitives available here: http_request, json_encode, json_decode,
-- hmac_sha256_base64, unix_timestamp, nonce (see
-- crates/core/src/distributor_lua.rs) -- no other network, crypto, or clock
-- access.

local BASE_URL = "https://open.jlcpcb.com"
local DETAIL_PATH = "/overseas/openapi/component/getComponentDetailByCode"

-- `HMAC-SHA256(secret, "METHOD\n{path}\n{timestamp}\n{nonce}\n{body}\n")`,
-- base64-encoded. The exact string-to-sign format is what JLCPCB verifies
-- server-side, so it's pinned by sign_for_test below rather than only
-- exercised indirectly through a live request.
local function sign(secret_key, method, path, timestamp, nonce_val, body)
    local string_to_sign = method .. "\n" .. path .. "\n" .. timestamp .. "\n" .. nonce_val .. "\n" .. body .. "\n"
    return hmac_sha256_base64(secret_key, string_to_sign)
end

function sign_for_test(req)
    return sign(req.secret_key, req.method, req.path, req.timestamp, req.nonce, req.body)
end

-- Parses a raw `getComponentDetailByCode` JSON response. Pure -- no network
-- -- so it's directly testable against a fixture.
function parse_component_response(json_text)
    local json = json_decode(json_text)
    local list = json.data
    if not list or #list == 0 then
        return nil
    end
    local c = list[1]

    local parameters = {}
    if c.parameters then
        for _, p in ipairs(c.parameters) do
            if p.parameterName and p.parameterValue then
                parameters[#parameters + 1] = { p.parameterName, p.parameterValue }
            end
        end
    end

    return {
        component_code = c.componentCode or "",
        component_model = c.componentModel or "",
        package = c.componentSpecification,
        description = c.description,
        -- Prefer the LCSC datasheet link, fall back to JLCPCB's file link.
        datasheet_url = c.dataManualUrl or c.datasheetUrl,
        library_type = c.libraryType,
        stock = c.stockCount,
        parameters = parameters,
    }
end

-- Sign and POST a request, returning the raw JSON response body (after
-- checking `code`, JLCPCB's own success/error signal -- independent of HTTP
-- status).
local function post(creds, path, body)
    local timestamp = tostring(unix_timestamp())
    local nonce_val = nonce()
    local signature = sign(creds.secret_key, "POST", path, timestamp, nonce_val, body)
    local auth = string.format(
        'JOP appid="%s",accesskey="%s",timestamp="%s",nonce="%s",signature="%s"',
        creds.app_id, creds.access_key, timestamp, nonce_val, signature
    )

    local resp = http_request({
        method = "POST",
        url = BASE_URL .. path,
        headers = {
            ["Content-Type"] = "application/json",
            ["Authorization"] = auth,
        },
        body = body,
    })
    local json = json_decode(resp.body)
    local code = json and json.code or 0
    if code ~= 200 then
        local message = (json and json.message) or "unknown error"
        error("JLCPCB API error (" .. tostring(code) .. "): " .. message)
    end
    return resp.body
end

function component_by_code(req)
    local body = json_encode({ componentCodes = { req.code } })
    local response_body = post(req, DETAIL_PATH, body)
    return parse_component_response(response_body)
end
